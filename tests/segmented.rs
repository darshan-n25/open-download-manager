//! End-to-end tests against a local HTTP server that really does byte ranges.
//!
//! These cover the parts that unit tests cannot: that the engine plans several ranged
//! requests that tile the file exactly, that a truncated response is detected and does not
//! produce a corrupt file, that the partial data is reused on the next run, and that a
//! server without range support still yields correct bytes in single-stream mode.

// Test fixtures are built field-by-field from `Default::default()`, which reads better
// than a fourteen-field struct literal.
#![allow(clippy::field_reassign_with_default)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use odm::config::Config;
use odm::engine::Engine;
use odm::grabber::{self, GrabberConfig};
use odm::job::{Job, JobKind, JobState};
use odm::scheduler::Scheduler;

/// Sentinel for "send the whole slice".
const NO_TRUNCATE: usize = usize::MAX;

struct TestServer {
    addr: std::net::SocketAddr,
    routes: Arc<HashMap<String, Arc<Vec<u8>>>>,
    ranges: Arc<Mutex<Vec<(usize, usize)>>>,
    bytes_served: Arc<AtomicU64>,
    requests: Arc<AtomicUsize>,
    in_flight: Arc<AtomicUsize>,
    peak_in_flight: Arc<AtomicUsize>,
    truncate: Arc<AtomicUsize>,
    support_ranges: Arc<AtomicBool>,
}

impl TestServer {
    async fn start(body: Vec<u8>) -> Arc<TestServer> {
        TestServer::start_with(vec![("/file.bin".to_string(), body)]).await
    }

    async fn start_with(routes: Vec<(String, Vec<u8>)>) -> Arc<TestServer> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let routes: Arc<HashMap<String, Arc<Vec<u8>>>> =
            Arc::new(routes.into_iter().map(|(k, v)| (k, Arc::new(v))).collect());
        let srv = Arc::new(TestServer {
            addr,
            routes: routes.clone(),
            ranges: Arc::new(Mutex::new(Vec::new())),
            bytes_served: Arc::new(AtomicU64::new(0)),
            requests: Arc::new(AtomicUsize::new(0)),
            in_flight: Arc::new(AtomicUsize::new(0)),
            peak_in_flight: Arc::new(AtomicUsize::new(0)),
            truncate: Arc::new(AtomicUsize::new(NO_TRUNCATE)),
            support_ranges: Arc::new(AtomicBool::new(true)),
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
                    // Every response here ends with `Connection: close`, so one accepted
                    // connection is one in-flight request. The socket is held open until
                    // after the count is decremented, so the client cannot observe EOF and
                    // start a replacement while this one is still counted.
                    let now = s2.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    s2.peak_in_flight.fetch_max(now, Ordering::SeqCst);
                    serve(&mut sock, s2.clone()).await;
                    s2.in_flight.fetch_sub(1, Ordering::SeqCst);
                    drop(sock);
                });
            }
        });
        srv
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn file_url(&self) -> String {
        self.url("/file.bin")
    }

    fn observed_ranges(&self) -> Vec<(usize, usize)> {
        self.ranges.lock().unwrap().clone()
    }

    fn bytes_served(&self) -> u64 {
        self.bytes_served.load(Ordering::SeqCst)
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    /// The most responses that were ever open at the same time.
    fn peak_in_flight(&self) -> usize {
        self.peak_in_flight.load(Ordering::SeqCst)
    }

    /// Make range responses deliver only `n` bytes before closing — a flaky-server simulation.
    fn truncate_at(&self, n: Option<usize>) {
        self.truncate
            .store(n.unwrap_or(NO_TRUNCATE), Ordering::SeqCst);
    }

    fn ignore_ranges(&self) {
        self.support_ranges.store(false, Ordering::SeqCst);
    }
}

/// `bytes=start-end` → inclusive bounds, clamped to the entity.
fn parse_range(value: &str, total: usize) -> Option<(usize, usize)> {
    let spec = value.trim().strip_prefix("bytes=")?;
    let first = spec.split(',').next()?.trim();
    let (s, e) = first.split_once('-')?;
    if s.trim().is_empty() {
        return None; // suffix ranges (`bytes=-500`) are not exercised here
    }
    let start: usize = s.trim().parse().ok()?;
    if start >= total {
        return None;
    }
    let end = if e.trim().is_empty() {
        total - 1
    } else {
        e.trim().parse::<usize>().ok()?.min(total - 1)
    };
    if start > end {
        return None;
    }
    Some((start, end))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

async fn serve(sock: &mut TcpStream, srv: Arc<TestServer>) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = match sock.read(&mut tmp).await {
            Ok(0) => return,
            Ok(n) => n,
            Err(_) => return,
        };
        buf.extend_from_slice(&tmp[..n]);
        if find_subslice(&buf, b"\r\n\r\n").is_some() {
            break;
        }
        if buf.len() > 64 * 1024 {
            return;
        }
    }

    let head = String::from_utf8_lossy(&buf).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let path = target.split(['?', '#']).next().unwrap_or("/").to_string();

    let mut range_header = None;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("range") {
                range_header = Some(v.trim().to_string());
            }
        }
    }

    srv.requests.fetch_add(1, Ordering::SeqCst);

    let Some(body) = srv.routes.get(&path).cloned() else {
        let _ = sock
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
        let _ = sock.flush().await;
        return;
    };
    let total = body.len();
    let supports = srv.support_ranges.load(Ordering::SeqCst);

    if method == "HEAD" {
        let ranges_hdr = if supports {
            "Accept-Ranges: bytes\r\n"
        } else {
            ""
        };
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
             Content-Length: {total}\r\n{ranges_hdr}ETag: \"ODM-Test-ETag\"\r\n\
             Connection: close\r\n\r\n"
        );
        let _ = sock.write_all(resp.as_bytes()).await;
        let _ = sock.flush().await;
        return;
    }

    let range = if supports {
        range_header.as_deref().and_then(|v| parse_range(v, total))
    } else {
        None
    };

    let (status_line, slice, content_range) = match range {
        Some((s, e)) => {
            srv.ranges.lock().unwrap().push((s, e));
            (
                "HTTP/1.1 206 Partial Content\r\n".to_string(),
                &body[s..=e],
                Some(format!("Content-Range: bytes {s}-{e}/{total}\r\n")),
            )
        }
        None => ("HTTP/1.1 200 OK\r\n".to_string(), &body[..], None),
    };

    let ranges_hdr = if supports {
        "Accept-Ranges: bytes\r\n"
    } else {
        ""
    };
    let mut resp = status_line;
    resp.push_str("Content-Type: application/octet-stream\r\n");
    resp.push_str(&format!("Content-Length: {}\r\n", slice.len()));
    resp.push_str(ranges_hdr);
    resp.push_str("ETag: \"ODM-Test-ETag\"\r\n");
    if let Some(cr) = content_range {
        resp.push_str(&cr);
    }
    resp.push_str("Connection: close\r\n\r\n");
    let _ = sock.write_all(resp.as_bytes()).await;

    let limit = srv.truncate.load(Ordering::SeqCst);
    let payload = if limit != NO_TRUNCATE && limit < slice.len() {
        &slice[..limit]
    } else {
        slice
    };
    let _ = sock.write_all(payload).await;
    let _ = sock.flush().await;
    srv.bytes_served
        .fetch_add(payload.len() as u64, Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn test_config(state_dir: &Path, download_dir: &Path) -> Config {
    let mut cfg = Config::default();
    cfg.state_dir = state_dir.to_path_buf();
    cfg.download_dir = download_dir.to_path_buf();
    cfg.connections = 4;
    cfg.min_chunk = 4096;
    cfg.retry_backoff_ms = 1;
    cfg.max_concurrent_jobs = 2;
    cfg.auto_categorize = false;
    cfg
}

fn make_job(url: &str, output: &Path, connections: usize, retries: u32) -> Job {
    let mut job = Job::new(
        "test".into(),
        url.to_string(),
        output.to_path_buf(),
        JobKind::Http,
    );
    job.connections = connections;
    job.min_chunk = 4096;
    job.max_retries = retries;
    job.output_provisional = false;
    job
}

/// Deterministic pseudo-random bytes so mismatches are obvious in a diff.
fn body_of(len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    while v.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.truncate(len);
    v
}

/// True when `ranges` tile `[0, total)` exactly once, with no gap and no overlap.
fn tiles_exactly(mut ranges: Vec<(usize, usize)>, total: usize) -> bool {
    if ranges.is_empty() {
        return false;
    }
    ranges.sort_unstable();
    let mut cursor = 0usize;
    for (s, e) in ranges {
        if s != cursor {
            return false;
        }
        cursor = e + 1;
    }
    cursor == total
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn segmented_download_tiles_the_file_and_matches_the_source() {
    let dir = tempfile::tempdir().unwrap();
    let data = body_of(4 << 20);
    let srv = TestServer::start(data.clone()).await;

    let cfg = test_config(&dir.path().join("state"), &dir.path().join("dl"));
    let out = dir.path().join("dl/file.bin");
    let sched = Scheduler::new(cfg).unwrap();
    let job = make_job(&srv.file_url(), &out, 4, 3);

    let finished = sched.execute(job).await.unwrap();
    assert_eq!(finished.state, JobState::Done);
    assert_eq!(finished.total, Some(data.len() as u64));
    assert_eq!(finished.downloaded, data.len() as u64);

    let got = std::fs::read(&out).unwrap();
    assert_eq!(got, data, "downloaded bytes must equal the source exactly");

    // Several ranged requests, and together they cover the file exactly once.
    let ranges = srv.observed_ranges();
    assert!(
        ranges.len() >= 2,
        "expected the file to be split across connections, saw {ranges:?}"
    );
    assert!(
        tiles_exactly(ranges.clone(), data.len()),
        "ranged requests did not tile the file: {ranges:?}"
    );

    // The part file is renamed away on success, not left behind.
    assert!(!out.with_extension("bin.part").exists());
    assert_eq!(sched.store.list().unwrap().len(), 1);
}

#[tokio::test]
async fn a_truncating_server_fails_cleanly_then_the_next_run_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let data = body_of(2 << 20);
    let srv = TestServer::start(data.clone()).await;
    let cfg = test_config(&dir.path().join("state"), &dir.path().join("dl"));
    let out = dir.path().join("dl/file.bin");
    let sched = Scheduler::new(cfg).unwrap();

    // --- Run 1: the server dies part-way through every response -------------------
    srv.truncate_at(Some(4096));
    let job = make_job(&srv.file_url(), &out, 4, 1);
    let id = job.id.clone();

    let first = sched.execute(job).await;
    assert!(
        first.is_err(),
        "a truncating server must not report success"
    );
    let stored = sched.store.load(&id).unwrap();
    assert_eq!(stored.state, JobState::Failed);
    assert!(stored.error.is_some());
    assert!(
        !out.exists(),
        "a failed download must not publish a partial file as final"
    );
    assert!(
        stored.downloaded > 0,
        "partial progress should have been recorded (got {})",
        stored.downloaded
    );

    // --- Run 2: the server recovers ----------------------------------------------
    srv.truncate_at(None);
    let before = srv.bytes_served();

    let resumed = sched.store.load(&id).unwrap();
    let finished = sched.execute(resumed).await.unwrap();
    assert_eq!(finished.state, JobState::Done);
    assert_eq!(
        std::fs::read(&out).unwrap(),
        data,
        "resumed bytes must be correct"
    );

    let after = srv.bytes_served();
    assert!(
        after - before < data.len() as u64,
        "the second run should have reused the partial file, but served {} bytes of {}",
        after - before,
        data.len()
    );
}

#[tokio::test]
async fn the_download_quota_pauses_the_job_and_a_later_window_resumes_it() {
    let dir = tempfile::tempdir().unwrap();
    let data = body_of(2 << 20);
    let srv = TestServer::start(data.clone()).await;

    // A budget far smaller than the file, so the transfer has to stop part-way.
    let mut cfg = test_config(&dir.path().join("state"), &dir.path().join("dl"));
    cfg.quota_bytes = Some(256 << 10);
    cfg.quota_window_secs = 3600;
    let out = dir.path().join("dl/file.bin");
    let sched = Scheduler::new(cfg).unwrap();

    let job = make_job(&srv.file_url(), &out, 4, 2);
    let id = job.id.clone();

    let first = sched.execute(job).await;
    assert!(first.is_err(), "the budget cannot cover the whole file");

    let stored = sched.store.load(&id).unwrap();
    assert_eq!(
        stored.state,
        JobState::Paused,
        "a spent quota must pause rather than fail: {:?} / {:?}",
        stored.state,
        stored.error
    );
    assert!(stored.quota_paused, "the job should be marked quota-paused");
    assert!(
        stored
            .error
            .as_deref()
            .unwrap_or("")
            .contains("download quota"),
        "the message should say why: {:?}",
        stored.error
    );
    assert!(
        !out.exists(),
        "a paused download must not publish a partial file as final"
    );
    assert!(stored.downloaded > 0, "some progress must have been made");
    assert!(
        stored.downloaded < data.len() as u64,
        "the quota was supposed to stop it early, but {} of {} bytes arrived",
        stored.downloaded,
        data.len()
    );
    assert!(
        stored.part_path().exists(),
        "the partial file must survive for the resume"
    );

    // --- A later window: fresh scheduler, budget refilled --------------------------
    let mut cfg2 = test_config(&dir.path().join("state"), &dir.path().join("dl"));
    cfg2.quota_bytes = None;
    let sched2 = Scheduler::new(cfg2).unwrap();

    let resumed = sched2.store.load(&id).unwrap();
    assert!(
        resumed.quota_paused,
        "the marker persists until the job completes"
    );
    let finished = sched2.execute(resumed).await.unwrap();
    assert_eq!(finished.state, JobState::Done);
    assert!(!finished.quota_paused, "completing clears the marker");
    assert_eq!(
        std::fs::read(&out).unwrap(),
        data,
        "resumed bytes must be correct"
    );
}

#[tokio::test]
async fn a_restarted_scheduler_does_not_hand_back_a_fresh_budget() {
    let dir = tempfile::tempdir().unwrap();
    let data = body_of(2 << 20);
    let srv = TestServer::start(data.clone()).await;

    let mut cfg = test_config(&dir.path().join("state"), &dir.path().join("dl"));
    cfg.quota_bytes = Some(256 << 10);
    cfg.quota_window_secs = 3600;
    let out = dir.path().join("dl/file.bin");

    // --- Run 1: spend the budget, and pause ---------------------------------------
    let sched = Scheduler::new(cfg.clone()).unwrap();
    let job = make_job(&srv.file_url(), &out, 4, 2);
    let id = job.id.clone();
    assert!(sched.execute(job).await.is_err());
    let first = sched.store.load(&id).unwrap();
    assert_eq!(first.state, JobState::Paused);
    let spent = first.downloaded;
    assert!(spent > 0, "run 1 should have transferred something");
    drop(sched);

    // --- Run 2: a whole new scheduler, same state directory, same budget -----------
    // A quota that vanished on restart would be trivially bypassed, so this is the point
    // of the feature rather than a detail.
    let restarted = Scheduler::new(cfg).unwrap();
    assert!(restarted.execute(first).await.is_err());
    let second = restarted.store.load(&id).unwrap();
    assert_eq!(second.state, JobState::Paused);
    // Run 1 stopped when no *whole* chunk would still fit, so a little budget was left
    // unspent and run 2 may use it. What it must not do is start a fresh window: that
    // would put it near 500 KB (two budgets' worth) instead of just over one.
    let budget = 256u64 << 10;
    assert!(
        second.downloaded <= budget + budget / 2,
        "after a restart the job reached {} bytes, which looks like a refreshed budget \
         rather than a remembered one (run 1 spent {spent})",
        second.downloaded
    );

    // --- Run 3: only lifting the budget lets it finish, byte for byte ---------------
    let mut lifted = test_config(&dir.path().join("state"), &dir.path().join("dl"));
    lifted.quota_bytes = None;
    let finisher = Scheduler::new(lifted).unwrap();
    let done = finisher.execute(second).await.unwrap();
    assert_eq!(done.state, JobState::Done);
    assert_eq!(std::fs::read(&out).unwrap(), data);
}

#[tokio::test]
async fn a_server_without_range_support_still_produces_correct_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let data = body_of(300 << 10);
    let srv = TestServer::start(data.clone()).await;
    srv.ignore_ranges();

    let cfg = test_config(&dir.path().join("state"), &dir.path().join("dl"));
    let out = dir.path().join("dl/file.bin");
    let sched = Scheduler::new(cfg).unwrap();
    let job = make_job(&srv.file_url(), &out, 4, 2);

    let finished = sched.execute(job).await.unwrap();
    assert_eq!(finished.state, JobState::Done);
    assert!(
        !finished.resumable,
        "a server without range support cannot be resumable"
    );
    assert_eq!(std::fs::read(&out).unwrap(), data);
    assert_eq!(
        srv.observed_ranges().len(),
        0,
        "the engine must not rely on ranges when the server refuses them"
    );
    // At least a HEAD probe and a GET.
    assert!(srv.requests() >= 2, "saw {} requests", srv.requests());
}

#[tokio::test]
async fn probe_reports_size_and_resumability() {
    let dir = tempfile::tempdir().unwrap();
    let data = body_of(128 << 10);
    let srv = TestServer::start(data.clone()).await;

    let cfg = test_config(&dir.path().join("state"), &dir.path().join("dl"));
    let job = make_job(&srv.file_url(), &dir.path().join("dl/file.bin"), 4, 3);

    let mut engine = Engine::new(cfg, job).unwrap();
    engine.probe().await.unwrap();

    let j = engine.job();
    assert_eq!(j.total, Some(data.len() as u64));
    assert!(j.resumable);
    assert_eq!(j.etag.as_deref(), Some("\"ODM-Test-ETag\""));
    assert_eq!(j.kind, JobKind::Http);
}

#[tokio::test]
async fn a_directory_url_resolves_to_an_index_like_filename() {
    let dir = tempfile::tempdir().unwrap();
    let data = body_of(64 << 10);
    let srv = TestServer::start_with(vec![("/payload".to_string(), data.clone())]).await;

    let cfg = test_config(&dir.path().join("state"), &dir.path().join("dl"));
    // No explicit output: the engine must derive one from the URL.
    let job = Job::new(
        "named".into(),
        srv.url("/payload"),
        dir.path().join("dl/unused"),
        JobKind::Http,
    );
    let mut job = job;
    job.output_provisional = true;
    job.connections = 2;

    let sched = Scheduler::new(cfg).unwrap();
    let finished = sched.execute(job).await.unwrap();
    assert_eq!(finished.state, JobState::Done);
    assert!(
        finished.output.ends_with("payload"),
        "expected the URL's basename, got {}",
        finished.output.display()
    );
    assert_eq!(std::fs::read(&finished.output).unwrap(), data);
}

#[tokio::test]
async fn the_grabber_finds_media_links_and_skips_trackers() {
    let dir = tempfile::tempdir().unwrap();

    let html = br#"
        <html><body>
          <a href="/movie.mp4">a movie</a>
          <a href="/archive.zip">an archive</a>
          <a href="/notes.txt">plain notes</a>
          <img src="/pixel.gif" width="1" height="1">
          <a href="https://www.google-analytics.com/collect">tracker</a>
          <a href="/nested/page.html">deeper</a>
        </body></html>
    "#
    .to_vec();

    let srv = TestServer::start_with(vec![
        ("/index.html".to_string(), html),
        ("/movie.mp4".to_string(), body_of(1024)),
        ("/archive.zip".to_string(), body_of(2048)),
        ("/notes.txt".to_string(), b"notes".to_vec()),
        (
            "/nested/page.html".to_string(),
            b"<a href=\"/deep.mp4\">x</a>".to_vec(),
        ),
        ("/deep.mp4".to_string(), body_of(512)),
    ])
    .await;

    let cfg = test_config(&dir.path().join("state"), &dir.path().join("dl"));
    let job = Job::new(
        "grab".into(),
        srv.url("/index.html"),
        dir.path().join(".probe"),
        JobKind::Grabber,
    );
    let engine = Engine::new(cfg, job).unwrap();
    let fetcher = engine.fetcher();

    // depth 0: only the start page is inspected.
    let shallow = grabber::crawl(
        &fetcher,
        &srv.url("/index.html"),
        &GrabberConfig {
            depth: 0,
            media_only: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let urls: Vec<&str> = shallow.iter().map(|t| t.url.as_str()).collect();
    assert!(
        urls.contains(&srv.url("/movie.mp4").as_str()),
        "saw {urls:?}"
    );
    assert!(
        urls.contains(&srv.url("/archive.zip").as_str()),
        "saw {urls:?}"
    );
    assert!(
        !urls.iter().any(|u| u.contains("google-analytics")),
        "trackers must be filtered: {urls:?}"
    );
    assert!(
        !urls.iter().any(|u| u.ends_with(".gif")),
        "images are not grabber targets: {urls:?}"
    );
    assert!(
        !urls.iter().any(|u| u.ends_with(".txt")),
        "non-media, non-archive types are left alone: {urls:?}"
    );

    // depth 1: the nested page is followed, revealing the deeper media link.
    let deep = grabber::crawl(
        &fetcher,
        &srv.url("/index.html"),
        &GrabberConfig {
            depth: 1,
            media_only: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let deep_urls: Vec<&str> = deep.iter().map(|t| t.url.as_str()).collect();
    assert!(
        deep_urls.contains(&srv.url("/deep.mp4").as_str()),
        "depth 1 should follow into the nested page: {deep_urls:?}"
    );
}

#[tokio::test]
async fn a_per_server_ceiling_caps_parallelism_within_one_job() {
    let dir = tempfile::tempdir().unwrap();
    let data = body_of(2 << 20);
    let srv = TestServer::start(data.clone()).await;

    let mut cfg = test_config(&dir.path().join("state"), &dir.path().join("dl"));
    // Ask for eight connections, but let the host accept only two at a time.
    cfg.per_server_connections = 2;
    let out = dir.path().join("dl/file.bin");
    let sched = Scheduler::new(cfg).unwrap();
    let job = make_job(&srv.file_url(), &out, 8, 3);

    let finished = sched.execute(job).await.unwrap();
    assert_eq!(finished.state, JobState::Done);
    assert_eq!(std::fs::read(&out).unwrap(), data);

    let peak = srv.peak_in_flight();
    assert!(
        peak <= 2,
        "the server saw {peak} simultaneous connections; the ceiling was 2"
    );
    assert!(
        srv.requests() >= 4,
        "the ceiling should throttle requests, not remove them (saw {})",
        srv.requests()
    );
    assert!(
        peak >= 2,
        "both allowed slots should be used, so the transfer stays parallel (peak {peak})"
    );
}
