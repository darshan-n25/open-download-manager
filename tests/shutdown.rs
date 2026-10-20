//! What a service manager does to a running Open Download Manager.
//!
//! `docker stop`, `systemctl stop` and `kubectl delete` all send SIGTERM and then, once the
//! grace period is up, SIGKILL. These tests signal the compiled binary for real, because the
//! reaction under test belongs to the process: no in-process call stands in for it, and the
//! behaviour being replaced — dying on the signal's default action, mid-write — is exactly
//! what cannot be observed from inside.
//!
//! Unix only. Windows has no SIGTERM, so Ctrl-C is the whole story there.

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long the whole stop may take. Generous next to the daemon's own five-second grace,
/// strict enough to catch a stop that hangs.
const STOP_DEADLINE: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// An isolated daemon
// ---------------------------------------------------------------------------

/// A daemon with its own configuration file and its own state directory.
///
/// `ODM_CONFIG` and `HOME` are both set, and the XDG variables cleared, so a stray
/// configuration on the machine running the tests cannot change what is under test.
struct Fixture {
    dir: tempfile::TempDir,
    config: PathBuf,
    home: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let config = home.join("config.json");
        let data = serde_json::json!({
            "state_dir": dir.path().join("state"),
            "download_dir": dir.path().join("dl"),
            "auto_categorize": false,
        });
        std::fs::write(&config, serde_json::to_vec_pretty(&data).unwrap()).unwrap();
        Fixture { dir, config, home }
    }

    /// Any Open Download Manager command, with the fixture's environment — the isolation the daemon gets
    /// belongs to every command a service manager might run in its place.
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_odm-cli"));
        c.args(args)
            .env("ODM_CONFIG", &self.config)
            .env("HOME", &self.home)
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("ODM_DAEMON_TOKEN")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c
    }

    /// The daemon, with its output captured so the stop can be read back.
    fn daemon(&self, port: u16) -> Child {
        self.command(&["daemon", "--addr", &format!("127.0.0.1:{port}")])
            .spawn()
            .expect("the test binary should be runnable")
    }

    /// The queue as the CLI sees it, which is what a person would look at after a stop.
    fn list(&self) -> Vec<serde_json::Value> {
        let out = Command::new(env!("CARGO_BIN_EXE_odm-cli"))
            .args(["list", "--json"])
            .env("ODM_CONFIG", &self.config)
            .env("HOME", &self.home)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "`odm-cli list` failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }
}

/// A port nothing is listening on, for the daemon to take.
fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// Send `SIGTERM` / `SIGINT` to a process.
///
/// Through `kill(1)` rather than a `libc` dependency: this is the one thing the test needs
/// from the C library, and it is the same call the service managers make.
fn signal(pid: u32, name: &str) {
    let ok = Command::new("kill")
        .args([format!("-{name}"), pid.to_string()])
        .status()
        .expect("`kill` should be on PATH")
        .success();
    assert!(ok, "kill -{name} {pid} failed");
}

/// Wait for the process to end, or give up. `None` means it was still running.
fn wait_for_exit(child: &mut Child, within: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + within;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whatever the process wrote to a piped stream, once it has exited.
fn drain(pipe: Option<impl Read>) -> String {
    let mut buf = Vec::new();
    if let Some(mut p) = pipe {
        let _ = p.read_to_end(&mut buf);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

// ---------------------------------------------------------------------------
// A trickle server, so there is something in flight to interrupt
// ---------------------------------------------------------------------------

/// Serves one file, slowly, over ranged requests.
///
/// The slowness is the point: a download that finished before the signal would leave nothing
/// for the stop to interrupt, and the test would pass without exercising the cancel path.
struct Trickle {
    port: u16,
    total: u64,
    served: Arc<AtomicU64>,
}

impl Trickle {
    fn start(total: u64) -> Trickle {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = Arc::new(AtomicU64::new(0));
        let counter = served.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let counter = counter.clone();
                std::thread::spawn(move || serve_one(stream, total, &counter));
            }
        });
        Trickle {
            port,
            total,
            served,
        }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/big.bin", self.port)
    }

    fn served(&self) -> u64 {
        self.served.load(Ordering::SeqCst)
    }

    /// Wait until the daemon has really started pulling bytes, which is the moment the job
    /// is mid-transfer and a stop has something to interrupt.
    fn wait_for_traffic(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.served() < 128 * 1024 {
            assert!(
                Instant::now() < deadline,
                "the daemon never started downloading: only {} bytes served",
                self.served()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn serve_one(mut s: TcpStream, total: u64, served: &AtomicU64) {
    let Ok(req) = read_request(&mut s) else {
        return;
    };
    if req.starts_with("HEAD") {
        let _ = s.write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\n\
                 Content-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        return;
    }
    let (start, end) = range_of(&req).unwrap_or((0, total - 1));
    if s.write_all(
        format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\n\
             Content-Length: {}\r\nContent-Type: application/octet-stream\r\n\r\n",
            end + 1 - start
        )
        .as_bytes(),
    )
    .is_err()
    {
        return;
    }

    // 64 KiB every 30 ms. Fast enough that a chunk boundary arrives soon after the signal
    // (the engine checks for cancellation between reads), slow enough that the transfer is
    // still going when it does.
    const CHUNK: usize = 64 * 1024;
    let buf = vec![0x41u8; CHUNK];
    let mut pos = start;
    while pos <= end {
        let n = CHUNK.min((end - pos + 1) as usize);
        if s.write_all(&buf[..n]).is_err() {
            // The daemon cancelled and closed the connection. That is the expected ending.
            return;
        }
        served.fetch_add(n as u64, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(30));
        pos += n as u64;
    }
}

fn read_request(s: &mut TcpStream) -> std::io::Result<String> {
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if s.read(&mut byte)? == 0 {
            break;
        }
        buf.push(byte[0]);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// `Range: bytes=a-b` from a request, if it has one.
fn range_of(req: &str) -> Option<(u64, u64)> {
    let line = req
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("range:"))?;
    let spec = line.split_once(':')?.1.trim().trim_start_matches("bytes=");
    let (a, b) = spec.split_once('-')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

// ---------------------------------------------------------------------------
// A minimal HTTP client, so the test drives the daemon the way a script would
// ---------------------------------------------------------------------------

fn try_request(port: u16, method: &str, path: &str, body: Option<&str>) -> Option<(u16, String)> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
    if let Some(b) = body {
        req.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            b.len()
        ));
    }
    req.push_str("\r\n");
    if let Some(b) = body {
        req.push_str(b);
    }
    s.write_all(req.as_bytes()).ok()?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).ok()?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    Some((status, body))
}

fn wait_until_serving(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while try_request(port, "GET", "/health", None).is_none() {
        assert!(Instant::now() < deadline, "the daemon never bound its port");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Queue a job and return its id.
fn queue(port: u16, url: &str) -> String {
    let body = serde_json::json!({ "url": url }).to_string();
    let (status, text) = try_request(port, "POST", "/api/jobs", Some(&body)).unwrap();
    assert_eq!(status, 201, "the daemon refused the job: {text}");
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    v["id"].as_str().unwrap().to_string()
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// SIGTERM while a download is in flight: the process stops itself, exits 0, and the job is
/// left as a resumable partial rather than a failure.
///
/// This is the path `docker stop` takes, and the exit code matters: a process killed by the
/// signal has no exit code at all, which is what an orchestrator reports as a crash.
#[test]
fn sigterm_stops_the_daemon_cleanly_mid_download() {
    let f = Fixture::new();
    let trickle = Trickle::start(64 * 1024 * 1024);
    let port = free_port();
    let mut daemon = f.daemon(port);
    wait_until_serving(port);

    let id = queue(port, &trickle.url());
    trickle.wait_for_traffic();

    let started = Instant::now();
    signal(daemon.id(), "TERM");
    let status = wait_for_exit(&mut daemon, STOP_DEADLINE);
    let status = match status {
        Some(s) => s,
        None => {
            daemon.kill().ok();
            panic!("the daemon ignored SIGTERM for {STOP_DEADLINE:?}");
        }
    };
    let took = started.elapsed();
    let stderr = drain(daemon.stderr.take());

    assert_eq!(
        status.code(),
        Some(0),
        "a signalled stop should be a clean exit, not a signal death: {status:?}\n{stderr}"
    );
    assert!(
        stderr.contains("SIGTERM"),
        "the stop should say what stopped it: {stderr}"
    );
    // The daemon waits up to five seconds for work to save; being far under that means it
    // waited for the cancel rather than for the deadline.
    assert!(
        took < Duration::from_secs(5),
        "the stop took {took:?}; it should not have needed the grace period"
    );

    // Nothing is lost: the job survived as a partial, and it is resumable.
    let jobs = f.list();
    assert_eq!(
        jobs.len(),
        1,
        "the queue should still hold the job: {jobs:#?}"
    );
    let job = &jobs[0];
    assert_eq!(job["id"], serde_json::json!(id));
    assert_eq!(job["state"], serde_json::json!("paused"), "{job:#?}");
    let done = job["downloaded"].as_u64().unwrap();
    assert!(done > 0, "some bytes should have arrived: {job:#?}");
    assert!(
        done < trickle.total,
        "the download should have been interrupted, not finished: {job:#?}"
    );
}

/// Ctrl-C still does what it did before this change, and leaves the same clean exit.
#[test]
fn sigint_still_stops_the_daemon() {
    let f = Fixture::new();
    let port = free_port();
    let mut daemon = f.daemon(port);
    wait_until_serving(port);

    signal(daemon.id(), "INT");
    let status = wait_for_exit(&mut daemon, STOP_DEADLINE)
        .unwrap_or_else(|| panic!("the daemon ignored SIGINT for {STOP_DEADLINE:?}"));
    let stderr = drain(daemon.stderr.take());

    assert_eq!(status.code(), Some(0), "{status:?}\n{stderr}");
    assert!(stderr.contains("interrupted"), "{stderr}");
}

/// The same stop, for `odm-cli download` — what a one-shot job runs instead of the daemon: a
/// Kubernetes `Job`, a `systemd` oneshot unit, a `cron` entry.
///
/// The exit code matters as much as it does for the daemon, and for the same reason. A unit
/// with `Restart=on-failure` restarts when this exits non-zero, so reporting a stop the
/// supervisor itself asked for as a failure turns a clean shutdown into a restart loop. What
/// the job leaves behind is a resumable partial, which is the opposite of a failure.
#[test]
fn sigterm_stops_a_foreground_download_cleanly() {
    let f = Fixture::new();
    let trickle = Trickle::start(64 * 1024 * 1024);
    let mut download = f
        .command(&["download", &trickle.url()])
        .spawn()
        .expect("the test binary should be runnable");

    trickle.wait_for_traffic();
    let started = Instant::now();
    signal(download.id(), "TERM");
    let status = match wait_for_exit(&mut download, STOP_DEADLINE) {
        Some(s) => s,
        None => {
            download.kill().ok();
            panic!("the download ignored SIGTERM for {STOP_DEADLINE:?}");
        }
    };
    let took = started.elapsed();
    let stderr = drain(download.stderr.take());

    assert_eq!(
        status.code(),
        Some(0),
        "a stopped download should be a clean exit, not a failure: {status:?}\n{stderr}"
    );
    assert!(stderr.contains("SIGTERM"), "{stderr}");
    assert!(
        !stderr.contains("failed"),
        "a stop asked for by a signal must not be reported as a failure: {stderr}"
    );
    assert!(took < Duration::from_secs(5), "the stop took {took:?}");

    // And the job is where it was, not gone and not broken.
    let jobs = f.list();
    assert_eq!(
        jobs.len(),
        1,
        "the interrupted download should still be in the queue: {jobs:#?}"
    );
    let job = &jobs[0];
    assert_eq!(job["state"], serde_json::json!("paused"), "{job:#?}");
    let done = job["downloaded"].as_u64().unwrap();
    assert!(
        done > 0 && done < trickle.total,
        "the download should be a partial: {job:#?}"
    );
}

/// A stop with nothing running must not sit out the grace period.
#[test]
fn a_stop_with_an_idle_queue_is_immediate() {
    let f = Fixture::new();
    let port = free_port();
    let mut daemon = f.daemon(port);
    wait_until_serving(port);

    let started = Instant::now();
    signal(daemon.id(), "TERM");
    let status = wait_for_exit(&mut daemon, STOP_DEADLINE)
        .unwrap_or_else(|| panic!("the daemon ignored SIGTERM for {STOP_DEADLINE:?}"));

    assert_eq!(status.code(), Some(0));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "an idle stop should not wait: {:?}",
        started.elapsed()
    );
    let _ = f.path();
}
