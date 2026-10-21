//! Daemon protocol tests.
//!
//! Covers the two surfaces the browser extension and scripts use: the `MSG#` WebSocket
//! protocol on `plugin.v3.internetdownloadmanager.com`, and the plain JSON HTTP API.

// Building a test fixture field-by-field from `Default::default()` is clearer than a
// 12-argument literal.
#![allow(clippy::field_reassign_with_default)]

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use odm::auth::token::Auth;
use odm::config::Config;
use odm::daemon::{self, encode_frame, msg, parse_frame, Message, PROTOCOL_VERSION};
use odm::project::{Project, ProjectKind, ProjectStore};

/// The token the `*_with_token` fixtures require. A literal, not a generated one, so a test
/// that forgets to send it fails for the right reason.
const TOKEN: &str = "test-token-0123456789abcdef0123456789abcdef";

struct Running {
    addr: std::net::SocketAddr,
    state_dir: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

async fn start_daemon() -> Running {
    start_daemon_with(|_| {}).await
}

async fn start_daemon_with(tweak: impl FnOnce(&mut Config)) -> Running {
    start_daemon_full(tweak, None).await
}

/// A daemon that requires `TOKEN` on every request.
async fn start_daemon_with_token() -> Running {
    start_daemon_full(|_| {}, Some(std::sync::Arc::new(Auth::new(TOKEN)))).await
}

async fn start_daemon_full(
    tweak: impl FnOnce(&mut Config),
    auth: Option<std::sync::Arc<Auth>>,
) -> Running {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.state_dir = dir.path().join("state");
    cfg.download_dir = dir.path().join("dl");
    cfg.daemon_addr = "127.0.0.1:0".to_string();
    cfg.auto_categorize = false;
    tweak(&mut cfg);

    let (listener, sched, _stop) = daemon::bind(&cfg).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = daemon::serve_on(listener, sched, auth).await;
    });

    Running {
        addr,
        state_dir: cfg.state_dir.clone(),
        _dir: dir,
    }
}

fn ws_url(addr: std::net::SocketAddr) -> String {
    format!("ws://{addr}/?cid=odm-cli-test&rnd=1")
}

fn ws_url_with_token(addr: std::net::SocketAddr, token: &str) -> String {
    format!("ws://{addr}/?cid=odm-cli-test&rnd=1&token={token}")
}

#[tokio::test]
async fn the_extension_handshake_negotiates_the_expected_subprotocol() {
    let d = start_daemon().await;

    let mut req = ws_url(d.addr).into_client_request().unwrap();
    req.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("plugin.v3.internetdownloadmanager.com"),
    );
    let (mut ws, resp) = tokio_tungstenite::connect_async(req).await.unwrap();

    assert_eq!(
        resp.headers()
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok()),
        Some("plugin.v3.internetdownloadmanager.com"),
        "the daemon must echo the IDM subprotocol"
    );

    // HELLO -> HELLO_ACK, carrying the packed protocol version.
    ws.send(WsMessage::Text(encode_frame(&Message::new(0, msg::HELLO))))
        .await
        .unwrap();

    let reply = next_frame(&mut ws).await;
    assert_eq!(reply.mtype, msg::HELLO_ACK);
    assert_eq!(reply.id, 0);
    assert_eq!(
        reply.get("version"),
        Some(PROTOCOL_VERSION.to_string().as_str())
    );
    assert_eq!(reply.get("protocol"), Some(daemon::WS_PROTOCOL));
    assert!(reply.get("server").unwrap().starts_with("odm-cli/"));
}

#[tokio::test]
async fn ping_status_and_unknown_opcodes_behave() {
    let d = start_daemon().await;
    let mut ws = connect(d.addr).await;

    ws.send(WsMessage::Text(encode_frame(&Message::new(5, msg::PING))))
        .await
        .unwrap();
    let pong = next_frame(&mut ws).await;
    assert_eq!(pong.mtype, msg::PONG);
    assert_eq!(pong.id, 5);

    ws.send(WsMessage::Text(encode_frame(&Message::new(6, msg::STATUS))))
        .await
        .unwrap();
    let status = next_frame(&mut ws).await;
    assert_eq!(status.mtype, msg::STATUS_ACK);
    assert_eq!(status.get("total"), Some("0"));
    assert_eq!(status.get("active"), Some("0"));

    ws.send(WsMessage::Text(encode_frame(&Message::new(7, 4242))))
        .await
        .unwrap();
    let err = next_frame(&mut ws).await;
    assert_eq!(err.mtype, msg::ERROR);
    assert_eq!(err.get("code"), Some("404"));
}

#[tokio::test]
async fn add_queues_a_job_and_reports_its_id() {
    let d = start_daemon().await;
    let mut ws = connect(d.addr).await;

    // A URL that cannot resolve: the queueing path is what is under test here, not the
    // transfer (that is covered by the end-to-end tests in `segmented.rs`).
    let add = Message::new(1, msg::ADD)
        .with("url", "http://127.0.0.1:9/never.bin")
        .with("connections", "3")
        .with("referer", "http://host/page")
        .with("cookie", "session=abc;other=1")
        .with("user_agent", "TestAgent/1.0");
    ws.send(WsMessage::Text(encode_frame(&add))).await.unwrap();

    let ack = next_frame(&mut ws).await;
    assert_eq!(ack.mtype, msg::ADD_ACK);
    let id = ack
        .get("id")
        .expect("ADD_ACK carries the job id")
        .to_string();
    assert!(!id.is_empty());

    // The job must be visible through the HTTP API too, with its captured request context.
    let body = http_get(d.addr, "/api/jobs").await;
    let jobs: serde_json::Value = serde_json::from_str(&body).unwrap();
    let list = jobs.as_array().unwrap();
    assert_eq!(list.len(), 1, "one job should be queued: {body}");
    assert_eq!(list[0]["id"], serde_json::json!(id));
    assert_eq!(list[0]["url"], "http://127.0.0.1:9/never.bin");
    assert_eq!(list[0]["connections"], 3);
    assert_eq!(list[0]["ctx"]["cookie"], "session=abc;other=1");
    assert_eq!(list[0]["ctx"]["referer"], "http://host/page");
    assert_eq!(list[0]["ctx"]["user_agent"], "TestAgent/1.0");
}

#[tokio::test]
async fn add_without_a_url_is_rejected_with_an_error_frame() {
    let d = start_daemon().await;
    let mut ws = connect(d.addr).await;

    ws.send(WsMessage::Text(encode_frame(
        &Message::new(9, msg::ADD).with("bogus", "1"),
    )))
    .await
    .unwrap();

    let err = next_frame(&mut ws).await;
    assert_eq!(err.mtype, msg::ERROR);
    assert_eq!(err.id, 9);
    assert_eq!(err.get("code"), Some("400"));
}

#[tokio::test]
async fn several_frames_in_one_websocket_message_are_all_handled() {
    let d = start_daemon().await;
    let mut ws = connect(d.addr).await;

    let batched = format!(
        "{}{}",
        encode_frame(&Message::new(1, msg::PING)),
        encode_frame(&Message::new(2, msg::PING))
    );
    ws.send(WsMessage::Text(batched)).await.unwrap();

    let a = next_frame(&mut ws).await;
    let b = next_frame(&mut ws).await;
    assert_eq!(a.mtype, msg::PONG);
    assert_eq!(b.mtype, msg::PONG);
    assert_eq!((a.id, b.id), (1, 2));
}

#[tokio::test]
async fn the_http_api_creates_and_lists_jobs() {
    let d = start_daemon().await;

    let health = http_get(d.addr, "/health").await;
    assert_eq!(health, "ok");

    // A real destination: on Windows `/tmp/...` is not a path at all, and a test that hands
    // the daemon one is testing the test's assumption about the filesystem.
    let out_dir = tempfile::tempdir().unwrap();
    let output = out_dir.path().join("api-test.bin");
    let output_json = output.to_string_lossy().to_string();

    let payload = serde_json::json!({
        "url": "http://127.0.0.1:9/api.bin",
        "output": output_json,
        "connections": 5,
        "quality": "720",
        "proxy": "socks5://127.0.0.1:9150",
        "speed_limit": "2M",
        "auth": "bob:hunter2",
        "user_agent": "Agent/2"
    });
    let (status, body) = http_post(d.addr, "/api/jobs", &payload.to_string()).await;
    assert_eq!(status, 201, "body: {body}");
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    let one = http_get(d.addr, &format!("/api/jobs/{id}")).await;
    let job: serde_json::Value = serde_json::from_str(&one).unwrap();
    assert_eq!(job["output"], output_json);
    assert_eq!(job["connections"], 5);
    assert_eq!(job["quality"], "720");
    assert_eq!(job["speed_limit"], 2097152);
    assert_eq!(job["ctx"]["proxy"], "socks5://127.0.0.1:9150");
    assert_eq!(job["ctx"]["user_agent"], "Agent/2");
    assert!(job["ctx"]["auth"].is_object());

    // A malformed body and a missing URL are both client errors.
    let (status, _) = http_post(d.addr, "/api/jobs", "not json").await;
    assert_eq!(status, 400);
    let (status, _) = http_post(d.addr, "/api/jobs", "{}").await;
    assert_eq!(status, 400);

    let (status, _) = http_get_raw(d.addr, "/api/jobs/nope").await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn a_job_can_be_paused_resumed_and_removed_over_http() {
    let d = start_daemon().await;

    let out_dir = tempfile::tempdir().unwrap();
    let output = out_dir.path().join("actions.bin");
    let payload = serde_json::json!({
        "url": "http://127.0.0.1:9/actions.bin",
        "output": output.to_string_lossy(),
    });
    let (status, body) = http_post(d.addr, "/api/jobs", &payload.to_string()).await;
    assert_eq!(status, 201, "body: {body}");
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    // Pause: the job is recorded as `paused`, which is what makes it resumable.
    let (status, _) = http_post(d.addr, &format!("/api/jobs/{id}/pause"), "{}").await;
    assert_eq!(status, 200);
    let job: serde_json::Value =
        serde_json::from_str(&http_get(d.addr, &format!("/api/jobs/{id}")).await).unwrap();
    assert_eq!(job["state"], "paused");

    // Resume: back to `queued`, so `odm-cli queue` will pick it up.
    let (status, _) = http_post(d.addr, &format!("/api/jobs/{id}/resume"), "{}").await;
    assert_eq!(status, 200);
    let job: serde_json::Value =
        serde_json::from_str(&http_get(d.addr, &format!("/api/jobs/{id}")).await).unwrap();
    assert_eq!(job["state"], "queued");

    // Remove: gone from the queue entirely.
    let (status, _) = http_post(d.addr, &format!("/api/jobs/{id}/remove"), "{}").await;
    assert_eq!(status, 200);
    let (status, _) = http_get_raw(d.addr, &format!("/api/jobs/{id}")).await;
    assert_eq!(status, 404);

    // An unknown action is a 404, and an unknown job is a 404 too.
    let (status, _) = http_post(d.addr, &format!("/api/jobs/{id}/explode"), "{}").await;
    assert_eq!(status, 404);
    let (status, _) = http_post(d.addr, "/api/jobs/nosuchjob/pause", "{}").await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn the_queue_can_be_started_and_stopped_over_http() {
    let d = start_daemon().await;

    let (status, body) = http_post(d.addr, "/api/queue/start", "{}").await;
    assert_eq!(status, 202, "body: {body}");

    let (status, body) = http_post(d.addr, "/api/queue/stop", "{}").await;
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains("paused"), "body: {body}");
}

#[tokio::test]
async fn the_job_list_carries_a_live_transfer_rate() {
    let d = start_daemon().await;
    // Every job reports a speed, even a stopped one: the column has to have a number in
    // it rather than nothing, or the client has to guess what a missing field means.
    let body = http_get(d.addr, "/api/jobs").await;
    let jobs: serde_json::Value = serde_json::from_str(&body).unwrap();
    for job in jobs.as_array().unwrap() {
        assert!(job.get("speed_bps").is_some(), "no speed_bps in {job}");
    }
}

#[tokio::test]
async fn the_download_quota_is_reported_over_every_surface() {
    // 300 MB per 10 minutes: a value that cannot be confused with a default.
    let d = start_daemon_with(|cfg| {
        cfg.quota_bytes = Some(300 << 20);
        cfg.quota_window_secs = 600;
    })
    .await;
    let mut ws = connect(d.addr).await;

    // The handshake advertises the budget, so the extension can show it before any download.
    ws.send(WsMessage::Text(encode_frame(&Message::new(0, msg::HELLO))))
        .await
        .unwrap();
    let hello = next_frame(&mut ws).await;
    assert_eq!(hello.mtype, msg::HELLO_ACK);
    assert_eq!(
        hello.get("quota"),
        Some((300u64 << 20).to_string().as_str())
    );
    assert_eq!(hello.get("quotawin"), Some("600"));

    // The status frame reports consumption and the time left in the window.
    ws.send(WsMessage::Text(encode_frame(&Message::new(6, msg::STATUS))))
        .await
        .unwrap();
    let status = next_frame(&mut ws).await;
    assert_eq!(status.mtype, msg::STATUS_ACK);
    assert_eq!(status.get("quota_used"), Some("0"));
    assert_eq!(
        status.get("quota_total"),
        Some((300u64 << 20).to_string().as_str())
    );
    assert_eq!(status.get("quota_paused"), Some("0"));
    let reset: u64 = status.get("quota_reset").unwrap().parse().unwrap();
    assert!(reset <= 600 && reset > 590, "reset was {reset}");

    // And the JSON API exposes the same snapshot.
    let snap: serde_json::Value =
        serde_json::from_str(&http_get(d.addr, "/api/quota").await).unwrap();
    assert_eq!(snap["limit"], serde_json::json!(300u64 << 20));
    assert_eq!(snap["used"], serde_json::json!(0));
    assert_eq!(snap["remaining"], serde_json::json!(300u64 << 20));
}

#[tokio::test]
async fn with_no_quota_configured_the_surfaces_say_so() {
    let d = start_daemon().await;
    let mut ws = connect(d.addr).await;

    ws.send(WsMessage::Text(encode_frame(&Message::new(0, msg::HELLO))))
        .await
        .unwrap();
    let hello = next_frame(&mut ws).await;
    assert_eq!(hello.get("quota"), Some("0"), "0 means unlimited");

    let snap: serde_json::Value =
        serde_json::from_str(&http_get(d.addr, "/api/quota").await).unwrap();
    assert!(
        snap["limit"].is_null(),
        "unlimited must be null, not 0: {snap}"
    );
    assert!(snap["remaining"].is_null());
    assert!(snap["resets_in_secs"].is_null());
}

#[tokio::test]
async fn the_control_page_is_served() {
    let d = start_daemon().await;
    let (status, body) = http_get_raw(d.addr, "/").await;
    assert_eq!(status, 200);
    assert!(body.contains("Open Download Manager"));
    assert!(body.contains("/api/jobs"));
    assert!(
        body.contains("/api/quota"),
        "the control page should show the download budget too"
    );
    assert!(
        body.contains("/api/projects"),
        "the control page should list recurring projects too"
    );
}

#[tokio::test]
async fn the_http_api_lists_projects_and_refuses_names_it_does_not_have() {
    let d = start_daemon().await;

    let listed: serde_json::Value =
        serde_json::from_str(&http_get(d.addr, "/api/projects").await).unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 0, "nothing yet");

    // Put a project where the daemon looks for them.
    let store = ProjectStore::open_in_state_dir(&d.state_dir).unwrap();
    let mut p = Project::new(
        "nightly",
        ProjectKind::File,
        "http://example.test/f.bin",
        d.state_dir.join("f.bin"),
        3600,
    );
    p.next_run_ms = Some(odm::util::now_ms() + 600_000);
    store.save(&p).unwrap();

    let listed: serde_json::Value =
        serde_json::from_str(&http_get(d.addr, "/api/projects").await).unwrap();
    let arr = listed.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "nightly");
    assert_eq!(arr[0]["kind"], "file");
    // The display strings are derived server-side, so a page does not have to re-implement
    // "is it due".
    assert!(
        arr[0]["next_run_in"].as_str().unwrap().starts_with("in "),
        "next_run_in should read like a person wrote it: {}",
        arr[0]["next_run_in"]
    );
    assert_eq!(arr[0]["last_run"], "never");

    // An unknown project is a 404 — not a panic, and not a silent success.
    let (status, _) = http_post(d.addr, "/api/projects/nope/run", "").await;
    assert_eq!(status, 404);

    // So is an action the API does not define.
    let (status, _) = http_post(d.addr, "/api/projects/nightly/frobnicate", "").await;
    assert_eq!(status, 404);
}

// ---------------------------------------------------------------------------
// Bearer-token authentication
// ---------------------------------------------------------------------------

/// A raw request carrying an arbitrary header block, so a test can send (or omit) the
/// `Authorization` header without going through the helpers that hard-code it.
async fn request_with_headers(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> (u16, String) {
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\n");
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    let (status, body) = raw_request(addr, &head).await;
    (status, body)
}

#[tokio::test]
async fn every_route_requires_the_token_when_one_is_configured() {
    let d = start_daemon_with_token().await;

    for path in [
        "/",
        "/index.html",
        "/health",
        "/api/jobs",
        "/api/quota",
        "/api/projects",
    ] {
        let (status, _) = http_get_raw(d.addr, path).await;
        assert_eq!(
            status, 401,
            "{path} must not be reachable without the token"
        );
    }

    // A wrong token is not a token.
    let (status, _) = request_with_headers(
        d.addr,
        "GET",
        "/api/jobs",
        &[("Authorization", "Bearer not-the-token")],
    )
    .await;
    assert_eq!(status, 401);

    // The right one is.
    let (status, body) = request_with_headers(
        d.addr,
        "GET",
        "/api/jobs",
        &[("Authorization", &format!("Bearer {TOKEN}"))],
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body.trim(), "[]");

    // And so is a differently-cased scheme, which RFC 7235 says is the same scheme.
    let (status, _) = request_with_headers(
        d.addr,
        "GET",
        "/health",
        &[("Authorization", &format!("bearer {TOKEN}"))],
    )
    .await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn a_401_says_what_to_do_and_offers_the_scheme() {
    let d = start_daemon_with_token().await;
    let mut sock = TcpStream::connect(d.addr).await.unwrap();
    sock.write_all(b"GET /api/jobs HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut raw = Vec::new();
    sock.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();

    assert!(text.starts_with("HTTP/1.1 401 Unauthorized"), "{text}");
    assert!(
        text.to_ascii_lowercase()
            .contains("www-authenticate: bearer"),
        "a client should be told which scheme to use: {text}"
    );
    assert!(text.contains("ODM_DAEMON_TOKEN"), "{text}");
}

#[tokio::test]
async fn the_token_may_ride_in_the_query_string() {
    // The WebSocket handshake needs this — a browser cannot set a header on it — so the HTTP
    // side accepts it too rather than having two rules for one credential.
    let d = start_daemon_with_token().await;

    let (status, _) = http_get_raw(d.addr, &format!("/api/quota?token={TOKEN}")).await;
    assert_eq!(status, 200);

    let (status, _) = http_get_raw(d.addr, "/api/quota?token=wrong").await;
    assert_eq!(status, 401);

    // Either carrier is enough. They are the same secret, so a request that presents it twice —
    // once stale — is not punished for the stale copy.
    let (status, _) = request_with_headers(
        d.addr,
        "GET",
        "/api/quota?token=wrong",
        &[("Authorization", &format!("Bearer {TOKEN}"))],
    )
    .await;
    assert_eq!(status, 200, "a good header carries the request");

    let (status, _) = request_with_headers(
        d.addr,
        "GET",
        &format!("/api/quota?token={TOKEN}"),
        &[("Authorization", "Bearer wrong")],
    )
    .await;
    assert_eq!(
        status, 200,
        "a good query parameter carries the request too"
    );

    // Both wrong is still wrong.
    let (status, _) = request_with_headers(
        d.addr,
        "GET",
        "/api/quota?token=wrong",
        &[("Authorization", "Bearer wrong")],
    )
    .await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn the_websocket_handshake_is_refused_without_the_token() {
    let d = start_daemon_with_token().await;

    let mut req = ws_url(d.addr).into_client_request().unwrap();
    req.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("plugin.v3.internetdownloadmanager.com"),
    );
    let refused = tokio_tungstenite::connect_async(req).await;
    assert!(
        refused.is_err(),
        "an unauthenticated client must not get a socket at all"
    );

    // With the token in the query string, the same URL connects and the handshake works.
    let mut req = ws_url_with_token(d.addr, TOKEN)
        .into_client_request()
        .unwrap();
    req.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("plugin.v3.internetdownloadmanager.com"),
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws.send(WsMessage::Text(encode_frame(&Message::new(1, msg::HELLO))))
        .await
        .unwrap();
    assert_eq!(next_frame(&mut ws).await.mtype, msg::HELLO_ACK);
}

#[tokio::test]
async fn a_websocket_client_with_headers_may_use_the_bearer_form() {
    // The header form works on the handshake too, for a client that is not a browser.
    let d = start_daemon_with_token().await;
    let mut req = ws_url(d.addr).into_client_request().unwrap();
    req.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("plugin.v3.internetdownloadmanager.com"),
    );
    req.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {TOKEN}")).unwrap(),
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws.send(WsMessage::Text(encode_frame(&Message::new(1, msg::HELLO))))
        .await
        .unwrap();
    assert_eq!(next_frame(&mut ws).await.mtype, msg::HELLO_ACK);
}

#[tokio::test]
async fn an_unauthenticated_daemon_still_requires_nothing() {
    // The default must not change: no token configured, no token needed.
    let d = start_daemon().await;
    let (status, _) = http_get_raw(d.addr, "/api/jobs").await;
    assert_eq!(status, 200);
    let mut req = ws_url(d.addr).into_client_request().unwrap();
    req.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("plugin.v3.internetdownloadmanager.com"),
    );
    assert!(tokio_tungstenite::connect_async(req).await.is_ok());
}

#[tokio::test]
async fn a_post_that_a_web_page_could_send_is_refused() {
    // The content types a cross-origin `fetch` or a form can send without a preflight. The
    // daemon answers no preflight, so refusing these is what stops a page on another origin
    // from queueing downloads on a tokenless loopback daemon.
    let d = start_daemon().await;
    let body = r#"{"url":"http://127.0.0.1:9/csrf.bin"}"#;

    for content_type in [
        "text/plain",
        "application/x-www-form-urlencoded",
        "multipart/form-data; boundary=x",
    ] {
        let (status, _) = raw_request(
            d.addr,
            &format!(
                "POST /api/jobs HTTP/1.1\r\nHost: localhost\r\n\
                 Content-Type: {content_type}\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            ),
        )
        .await;
        assert_eq!(status, 415, "{content_type}");
    }

    // No header at all is no declaration.
    let (status, _) = raw_request(
        d.addr,
        &format!(
            "POST /api/jobs HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        ),
    )
    .await;
    assert_eq!(status, 415);

    // Nothing was queued by any of them.
    let (status, listed) = http_get_raw(d.addr, "/api/jobs").await;
    assert_eq!(status, 200);
    assert_eq!(listed.trim(), "[]");

    // A real client still works, header and all.
    let (status, _) = http_post(d.addr, "/api/jobs", body).await;
    assert_eq!(status, 201);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn connect(
    addr: std::net::SocketAddr,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>> {
    let mut req = ws_url(addr).into_client_request().unwrap();
    req.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("plugin.v3.internetdownloadmanager.com"),
    );
    let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws
}

async fn next_frame<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>) -> Message
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        let item = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for a frame")
            .expect("websocket closed")
            .expect("websocket error");
        let text = match item {
            WsMessage::Text(t) => t.to_string(),
            WsMessage::Binary(b) => String::from_utf8_lossy(&b).to_string(),
            _ => continue,
        };
        if let Some((m, _)) = parse_frame(&text) {
            return m;
        }
    }
}

/// Minimal HTTP/1.1 request: the daemon closes every connection after responding.
async fn raw_request(addr: std::net::SocketAddr, request: &str) -> (u16, String) {
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(request.as_bytes()).await.unwrap();
    sock.flush().await.unwrap();

    let mut buf = Vec::new();
    sock.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf).to_string();
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = match text.find("\r\n\r\n") {
        Some(i) => text[i + 4..].to_string(),
        None => String::new(),
    };
    (status, body)
}

async fn http_get_raw(addr: std::net::SocketAddr, path: &str) -> (u16, String) {
    raw_request(
        addr,
        &format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"),
    )
    .await
}

async fn http_get(addr: std::net::SocketAddr, path: &str) -> String {
    http_get_raw(addr, path).await.1
}

async fn http_post(addr: std::net::SocketAddr, path: &str, body: &str) -> (u16, String) {
    raw_request(
        addr,
        &format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
    .await
}
