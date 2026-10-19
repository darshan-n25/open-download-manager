//! End-to-end checksum verification.
//!
//! These stand up a real HTTP server (ranged and non-ranged) and check the guarantee the
//! feature is supposed to give: a download whose bytes do not match the expected digest is
//! **never** published, and an existing good file is never replaced by a bad one.

#![allow(clippy::field_reassign_with_default)]

use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use riptide::checksum::{Algorithm, Checksum};
use riptide::config::Config;
use riptide::engine::Engine;
use riptide::error::RiptideError;
use riptide::job::{Job, JobKind};

/// A deterministic body large enough to be split into many segments.
fn body(len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    let mut x: u32 = 0x1234_5678;
    for _ in 0..len {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        v.push((x >> 16) as u8);
    }
    v
}

/// A minimal HTTP server that serves one entity, with or without range support.
struct Server {
    addr: SocketAddr,
    ranges: Arc<AtomicBool>,
}

impl Server {
    async fn start(data: Vec<u8>, accept_ranges: bool) -> Arc<Server> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = Arc::new(Server {
            addr,
            ranges: Arc::new(AtomicBool::new(accept_ranges)),
        });
        let data = Arc::new(data);
        let accept = srv.ranges.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let data = data.clone();
                let accept = accept.clone();
                tokio::spawn(async move {
                    let _ = serve(&mut sock, &data, accept.load(Ordering::SeqCst)).await;
                });
            }
        });
        srv
    }

    fn url(&self) -> String {
        format!("http://{}/file.bin", self.addr)
    }
}

/// Read one request and answer it. `Connection: close` keeps this to one request per socket,
/// which is what the engine expects.
async fn serve(sock: &mut TcpStream, data: &[u8], accept_ranges: bool) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf).to_string();
    let range = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("range")
                .then(|| v.trim().to_string())
        })
        .filter(|_| accept_ranges);

    let total = data.len();
    let response = if let Some(spec) = range {
        let spec = spec.strip_prefix("bytes=").unwrap_or(&spec);
        let (s, e) = spec.split_once('-').unwrap_or(("0", ""));
        let start: usize = s.trim().parse().unwrap_or(0);
        let end: usize = e.trim().parse().unwrap_or(total - 1).min(total - 1);
        let slice = &data[start..=end];
        let mut head = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes \
             {start}-{end}/{total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
            slice.len()
        )
        .into_bytes();
        head.extend_from_slice(slice);
        head
    } else {
        let mut head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: {}\r\nConnection: \
             close\r\n\r\n",
            if accept_ranges { "bytes" } else { "none" }
        )
        .into_bytes();
        head.extend_from_slice(data);
        head
    };
    sock.write_all(&response).await?;
    sock.flush().await
}

fn test_config(state: &Path, download: &Path) -> Config {
    let mut cfg = Config::default();
    cfg.state_dir = state.to_path_buf();
    cfg.download_dir = download.to_path_buf();
    cfg.connections = 4;
    cfg.min_chunk = 4096;
    cfg.split_threshold = 8192;
    cfg.retry_backoff_ms = 1;
    cfg.auto_categorize = false;
    cfg
}

fn job(url: &str, output: &Path, checksum: Option<Checksum>) -> Job {
    let mut job = Job::new(
        "ck".into(),
        url.to_string(),
        output.to_path_buf(),
        JobKind::Http,
    );
    job.connections = 4;
    job.min_chunk = 4096;
    job.max_retries = 2;
    job.output_provisional = false;
    job.checksum = checksum;
    job
}

#[tokio::test]
async fn a_matching_checksum_publishes_the_file() {
    let data = body(200 * 1024);
    let srv = Server::start(data.clone(), true).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("file.bin");

    let good = Checksum::of_bytes(Algorithm::Sha256, &data);
    let cfg = test_config(dir.path(), dir.path());
    let mut engine = Engine::new(cfg, job(&srv.url(), &out, Some(good))).unwrap();
    engine.probe().await.unwrap();
    engine.run().await.unwrap();

    assert_eq!(std::fs::read(&out).unwrap(), data, "bytes must be exact");
    assert!(
        !dir.path().join("file.bin.part").exists(),
        "the working file must be gone after the rename"
    );
}

#[tokio::test]
async fn a_mismatching_checksum_fails_and_publishes_nothing() {
    let data = body(200 * 1024);
    let srv = Server::start(data.clone(), true).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("file.bin");

    let wrong = Checksum::of_bytes(Algorithm::Sha256, b"not the body");
    let cfg = test_config(dir.path(), dir.path());
    let mut engine = Engine::new(cfg, job(&srv.url(), &out, Some(wrong.clone()))).unwrap();
    engine.probe().await.unwrap();
    let err = engine.run().await.unwrap_err();

    match &err {
        RiptideError::ChecksumMismatch { expected, .. } => {
            assert_eq!(
                expected, &wrong.digest,
                "the error names the expected digest"
            );
        }
        other => panic!("expected a checksum mismatch, got {other}"),
    }
    assert!(!out.exists(), "a failed checksum must publish nothing");
    assert!(
        !dir.path().join("file.bin.part").exists(),
        "the working file must be removed on mismatch"
    );
    // And the job is recorded as failed, not done.
    assert_eq!(engine.job().state, riptide::job::JobState::Failed);
}

#[tokio::test]
async fn a_mismatch_never_replaces_an_existing_good_file() {
    let data = body(200 * 1024);
    let srv = Server::start(data.clone(), true).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("file.bin");
    std::fs::write(&out, b"a previous, known-good download").unwrap();

    let wrong = Checksum::of_bytes(Algorithm::Sha256, b"wrong");
    let cfg = test_config(dir.path(), dir.path());
    let mut engine = Engine::new(cfg, job(&srv.url(), &out, Some(wrong))).unwrap();
    engine.probe().await.unwrap();
    assert!(engine.run().await.is_err());

    assert_eq!(
        std::fs::read(&out).unwrap(),
        b"a previous, known-good download",
        "the existing file must be untouched"
    );
}

#[tokio::test]
async fn a_checksum_is_enforced_in_single_stream_mode_too() {
    let data = body(40 * 1024);
    // No `Accept-Ranges`, so the engine cannot segment and takes the single-stream path.
    let srv = Server::start(data.clone(), false).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("file.bin");

    let good = Checksum::of_bytes(Algorithm::Sha512, &data);
    let cfg = test_config(dir.path(), dir.path());
    let mut engine = Engine::new(cfg, job(&srv.url(), &out, Some(good))).unwrap();
    engine.probe().await.unwrap();
    engine.run().await.unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), data);

    // Same body, now with the wrong expectation.
    let out2 = dir.path().join("file2.bin");
    let bad = Checksum::of_bytes(Algorithm::Sha512, b"something else");
    let cfg = test_config(dir.path(), dir.path());
    let mut engine = Engine::new(cfg, job(&srv.url(), &out2, Some(bad))).unwrap();
    engine.probe().await.unwrap();
    assert!(engine.run().await.is_err());
    assert!(!out2.exists());
}

#[tokio::test]
async fn a_md5_and_sha1_expectation_are_honoured() {
    let data = body(64 * 1024);
    let srv = Server::start(data.clone(), true).await;
    let dir = tempfile::tempdir().unwrap();

    for alg in [Algorithm::Md5, Algorithm::Sha1] {
        let out = dir.path().join(format!("{}.bin", alg.as_str()));
        let good = Checksum::of_bytes(alg, &data);
        // Round-trip through the textual spec a user would type.
        let parsed = Checksum::parse(&good.spec()).unwrap();
        assert_eq!(parsed, good);

        let cfg = test_config(dir.path(), dir.path());
        let mut engine = Engine::new(cfg, job(&srv.url(), &out, Some(parsed))).unwrap();
        engine.probe().await.unwrap();
        engine.run().await.unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), data);
    }
}

#[tokio::test]
async fn a_job_without_a_checksum_is_unaffected() {
    let data = body(32 * 1024);
    let srv = Server::start(data.clone(), true).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("file.bin");

    let cfg = test_config(dir.path(), dir.path());
    let mut engine = Engine::new(cfg, job(&srv.url(), &out, None)).unwrap();
    engine.probe().await.unwrap();
    engine.run().await.unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), data);
}
