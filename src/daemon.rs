//! Localhost daemon: the endpoint a browser extension hands downloads to.
//!
//! IDM runs two channels. The browser extension's primary path is a WebSocket to
//! `ws://127.0.0.1:1001/?cid=...` negotiating the subprotocol
//! `plugin.v3.internetdownloadmanager.com`; its fallback is WebExtension native messaging
//! (`com.tonec.idm` → `IDMMsgHost.exe` → named pipe). The wire format is a custom text
//! protocol, not JSON:
//!
//! ```text
//! MSG#<msgId>#<type>#<fieldA>#<fieldB>[,<key>=<value>...][;]
//! ```
//!
//! (A real framed sample ships inside `IDMMsgHost.exe`: `MSG#0#17#1#0:9:`.)
//!
//! Riptide implements the same listen address, the same subprotocol, and the same framing,
//! with a small documented opcode set — see [`msg`]. It additionally exposes a plain JSON
//! HTTP API on the same port, so scripts and the CLI can drive it without a browser, and
//! so the protocol surface is testable with ordinary tools.
//!
//! Honest limitation: IDM's server-side opcode enum is not in the shipped extension (the
//! extension only contains the *dispatcher*), so only the subset Riptide itself defines is
//! guaranteed. The framing, the handshake shape and the subprotocol string are as observed.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::config::Config;
use crate::error::{Result, RiptideError};
use crate::job::{Job, JobKind, JobState, RequestContext};
use crate::project::ProjectRunner;
use crate::scheduler::Scheduler;

/// How often the daemon looks for projects whose scheduled time has arrived.
///
/// A poll rather than a timer set to the earliest `next_run_ms`: a poll is a read of a few
/// small files, it needs no coordination when the CLI edits a project behind the daemon's
/// back, and being up to this late is irrelevant for a job measured in minutes.
pub const PROJECT_TICK_SECS: u64 = 30;

/// The subprotocol string the shipped Chrome extension requests.
pub const WS_PROTOCOL: &str = "plugin.v3.internetdownloadmanager.com";

/// Opcode set. IDM's own numbering is not fully recoverable; these values are Riptide's
/// and are documented in `docs/FEATURE-MAP.md`.
pub mod msg {
    /// Browser → daemon: announce the extension and its protocol version.
    pub const HELLO: u32 = 1;
    /// Daemon → browser: handshake acknowledgement, carries capabilities.
    pub const HELLO_ACK: u32 = 2;
    /// Browser → daemon: queue a download.
    pub const ADD: u32 = 3;
    /// Daemon → browser: the queued job's id.
    pub const ADD_ACK: u32 = 4;
    pub const PING: u32 = 5;
    pub const PONG: u32 = 6;
    pub const STATUS: u32 = 7;
    pub const STATUS_ACK: u32 = 8;
    pub const ERROR: u32 = 9;
    pub const BYE: u32 = 10;
}

/// The packed version constant the shipped extension sends in its handshake.
///
/// Observed literally as the decimal `16845059` in `IDMGCEExt`'s bundled script
/// (`S(this, 2, 5, 0, [112, 91, 1028:1031, 0, 16845059, clientId, 15, manifestVersion], …)`),
/// which is `0x01010903`. Riptide accepts any version and only logs a mismatch, so a future
/// IDM build with a bumped constant still interoperates.
pub const PROTOCOL_VERSION: u64 = 16845059;

/// Upper bound the daemon accepts for `ADD`'s `connections`. A page cannot talk the daemon
/// into opening hundreds of sockets.
pub const MAX_CONNECTIONS_PER_FILE: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub id: u64,
    pub mtype: u32,
    pub a: u64,
    pub b: u64,
    pub kv: Vec<(String, String)>,
}

impl Message {
    pub fn new(id: u64, mtype: u32) -> Self {
        Message {
            id,
            mtype,
            a: 0,
            b: 0,
            kv: Vec::new(),
        }
    }

    pub fn with(mut self, k: &str, v: impl Into<String>) -> Self {
        self.kv.push((k.to_string(), v.into()));
        self
    }

    pub fn get(&self, k: &str) -> Option<&str> {
        self.kv
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    }
}

fn encode_component(s: &str) -> String {
    use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
    const SET: &AsciiSet = &CONTROLS.add(b',').add(b';').add(b'=').add(b'#').add(b'%');
    utf8_percent_encode(s, SET).to_string()
}

fn decode_component(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .to_string()
}

/// Serialise one frame.
pub fn encode_frame(m: &Message) -> String {
    let mut out = format!("MSG#{}#{}#{}#{}", m.id, m.mtype, m.a, m.b);
    if !m.kv.is_empty() {
        out.push(',');
        out.push_str(
            &m.kv
                .iter()
                .map(|(k, v)| format!("{}={}", encode_component(k), encode_component(v)))
                .collect::<Vec<_>>()
                .join(","),
        );
    }
    out.push(';');
    out
}

/// Parse the first frame in `input`, returning it and how many bytes it consumed.
///
/// Frames are `;`-terminated, so a WebSocket message or native-message may carry several,
/// and a frame may be split across messages — which is exactly how the shipped extension
/// behaves (it runs a resumable scanner over its receive buffer).
pub fn parse_frame(input: &str) -> Option<(Message, usize)> {
    let rest = input.strip_prefix("MSG#")?;
    let semi = rest.find(';')?;
    let body = &rest[..semi];
    let consumed = 4 + semi + 1;

    let (head, kv_part) = match body.find(',') {
        Some(i) => (&body[..i], Some(&body[i + 1..])),
        None => (body, None),
    };
    let mut it = head.split('#');
    let id: u64 = it.next()?.trim().parse().ok()?;
    let mtype: u32 = it.next()?.trim().parse().ok()?;
    let a: u64 = it.next().unwrap_or("0").trim().parse().unwrap_or(0);
    let b: u64 = it.next().unwrap_or("0").trim().parse().unwrap_or(0);

    let mut kv = Vec::new();
    if let Some(kvs) = kv_part {
        for pair in kvs.split(',') {
            if let Some((k, v)) = pair.split_once('=') {
                kv.push((decode_component(k.trim()), decode_component(v.trim())));
            }
        }
    }

    Some((
        Message {
            id,
            mtype,
            a,
            b,
            kv,
        },
        consumed,
    ))
}

/// Pull every complete frame out of a buffer, returning the leftover tail.
///
/// Bytes before the first `MSG#` marker are discarded (a stray native-messaging length
/// prefix, say), and an incomplete trailing frame is returned untouched so the caller can
/// complete it from the next message. Without the discard step a peer that never sends a
/// valid marker would grow the buffer without bound.
pub fn drain_frames(buf: &str) -> (Vec<Message>, String) {
    let mut out = Vec::new();
    let mut rest = buf;
    loop {
        if !rest.starts_with("MSG#") {
            match rest.find("MSG#") {
                Some(i) => rest = &rest[i..],
                None => return (out, String::new()),
            }
        }
        match parse_frame(rest) {
            Some((m, n)) => {
                out.push(m);
                rest = &rest[n..];
            }
            // An incomplete frame: hand it back verbatim.
            None => return (out, rest.to_string()),
        }
    }
}

/// Build a job from an [`msg::ADD`] frame.
pub fn job_from_message(cfg: &Config, m: &Message) -> Result<Job> {
    let url = m
        .get("url")
        .ok_or_else(|| RiptideError::other("ADD message has no url"))?
        .to_string();

    // An explicit `kind` wins; otherwise infer it from the URL, so a `.m3u8` handed over by
    // the browser extension is captured as HLS instead of fetched as an opaque stream.
    let kind = match m.get("kind").unwrap_or("") {
        "hls" => JobKind::Hls,
        "dash" => JobKind::Dash,
        "ftp" => JobKind::Ftp,
        "http" => JobKind::Http,
        _ => JobKind::guess(&url),
    };

    let id = crate::util::gen_id();
    let explicit = m.get("output").map(std::path::PathBuf::from);
    let provisional = explicit.is_none();
    let output = explicit.unwrap_or_else(|| cfg.download_dir.join(format!("{id}.download")));

    let mut job = Job::new(id, url, output, kind);
    job.output_provisional = provisional;
    job.connections = m
        .get("connections")
        .and_then(|v| v.parse().ok())
        .unwrap_or(cfg.connections)
        .clamp(1, MAX_CONNECTIONS_PER_FILE);
    // Accepts both a raw byte count and the `2M` shorthand the CLI uses.
    job.speed_limit = m.get("speed_limit").and_then(crate::util::parse_size);
    job.quality = m.get("quality").map(|s| s.to_string());
    if let Some(spec) = m.get("checksum") {
        job.checksum = Some(crate::checksum::Checksum::parse(spec)?);
    }
    job.min_chunk = cfg.min_chunk;
    job.max_retries = cfg.max_retries;

    let mut ctx = RequestContext {
        user_agent: m.get("user_agent").map(|s| s.to_string()),
        referer: m.get("referer").map(|s| s.to_string()),
        cookie: m.get("cookie").map(|s| s.to_string()),
        proxy: m.get("proxy").map(|s| s.to_string()),
        ..Default::default()
    };
    if let Some(auth) = m.get("auth").and_then(crate::auth::parse_auth_spec) {
        ctx.auth = Some(auth);
    }
    if let Some(extra) = m.get("headers_extra") {
        for line in extra.split('\n') {
            if let Some((k, v)) = line.split_once(':') {
                ctx.headers
                    .push((k.trim().to_string(), v.trim().to_string()));
            }
        }
    }
    job.ctx = ctx;
    Ok(job)
}

async fn respond(ws: &mut WsStream<'_>, m: Message) -> Result<()> {
    let text = encode_frame(&m);
    ws.send(tokio_tungstenite::tungstenite::Message::Text(text))
        .await
        .map_err(|e| RiptideError::other(format!("websocket send failed: {e}")))?;
    Ok(())
}

type WsStream<'a> = tokio_tungstenite::WebSocketStream<&'a mut TcpStream>;

async fn handle_ws(
    stream: &mut TcpStream,
    sched: Arc<Scheduler>,
    peer: std::net::SocketAddr,
) -> Result<()> {
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

    // The handshake callback's error type (`Response<Option<String>>`, from tungstenite) is
    // a fixed part of the library's signature and is never produced here.
    #[allow(clippy::result_large_err)]
    let mut ws =
        tokio_tungstenite::accept_hdr_async(&mut *stream, |req: &Request, mut resp: Response| {
            let requested = req
                .headers()
                .get("sec-websocket-protocol")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            let chosen = requested
                .split(',')
                .map(|s| s.trim())
                .find(|p| *p == WS_PROTOCOL)
                .unwrap_or(WS_PROTOCOL);
            // Build a static string for the header: the value must live as long as the response.
            let value = if chosen == WS_PROTOCOL {
                tokio_tungstenite::tungstenite::http::HeaderValue::from_static(WS_PROTOCOL)
            } else {
                tokio_tungstenite::tungstenite::http::HeaderValue::from_str(chosen).unwrap_or_else(
                    |_| tokio_tungstenite::tungstenite::http::HeaderValue::from_static(WS_PROTOCOL),
                )
            };
            resp.headers_mut().insert("sec-websocket-protocol", value);
            Ok(resp)
        })
        .await
        .map_err(|e| RiptideError::other(format!("websocket handshake failed: {e}")))?;

    tracing::info!(%peer, protocol = WS_PROTOCOL, "extension connected");

    let mut buffer = String::new();

    while let Some(incoming) = ws.next().await {
        let incoming = match incoming {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(%peer, error = %e, "websocket receive failed");
                break;
            }
        };
        match incoming {
            tokio_tungstenite::tungstenite::Message::Text(t) => buffer.push_str(&t),
            tokio_tungstenite::tungstenite::Message::Binary(b) => {
                buffer.push_str(&String::from_utf8_lossy(&b))
            }
            tokio_tungstenite::tungstenite::Message::Close(_) => break,
            tokio_tungstenite::tungstenite::Message::Ping(_)
            | tokio_tungstenite::tungstenite::Message::Pong(_)
            | tokio_tungstenite::tungstenite::Message::Frame(_) => continue,
        }

        let (frames, tail) = drain_frames(&buffer);
        buffer = tail;

        for frame in frames {
            match frame.mtype {
                msg::HELLO => {
                    // Tolerant on purpose: a mismatch is worth knowing about, but not worth
                    // refusing to download over.
                    let theirs = frame
                        .get("version")
                        .and_then(|v| v.parse::<u64>().ok())
                        .unwrap_or(0);
                    if theirs != PROTOCOL_VERSION {
                        tracing::debug!(
                            theirs,
                            ours = PROTOCOL_VERSION,
                            "extension protocol version differs; continuing"
                        );
                    }
                    let reply = Message::new(frame.id, msg::HELLO_ACK)
                        .with("version", PROTOCOL_VERSION.to_string())
                        .with("protocol", WS_PROTOCOL)
                        .with("conn", sched.cfg.connections.to_string())
                        .with("perconn", sched.cfg.per_server_connections.to_string())
                        .with("maxconn", MAX_CONNECTIONS_PER_FILE.to_string())
                        .with("hls", "1")
                        .with("dash", "1")
                        .with("ftp", "1")
                        // Our own additions; IDM has no equivalent handshake field. `quota`
                        // is 0 when unlimited, which is the common case.
                        .with("quota", sched.quota.limit_bytes().unwrap_or(0).to_string())
                        .with("quotawin", sched.quota.window().as_secs().to_string())
                        .with("server", format!("riptide/{}", env!("CARGO_PKG_VERSION")));
                    respond(&mut ws, reply).await?;
                }
                msg::PING => {
                    respond(&mut ws, Message::new(frame.id, msg::PONG)).await?;
                }
                msg::ADD => match job_from_message(&sched.cfg, &frame) {
                    Ok(job) => {
                        let id = job.id.clone();
                        let url = job.url.clone();
                        // Queue it: the engine claims a slot when the queue allows.
                        let handle = sched.spawn(job);
                        let log_id = id.clone();
                        tokio::spawn(async move {
                            match handle.await {
                                Ok(Ok(j)) => {
                                    tracing::info!(job = %j.id, "queued download finished")
                                }
                                Ok(Err(e)) => {
                                    tracing::warn!(job = %log_id, error = %e, "queued download failed")
                                }
                                Err(e) => {
                                    tracing::error!(job = %log_id, error = %e, "worker task failed")
                                }
                            }
                        });
                        let reply = Message::new(frame.id, msg::ADD_ACK)
                            .with("id", id)
                            .with("url", url)
                            .with("next", (frame.id + 1).to_string());
                        respond(&mut ws, reply).await?;
                    }
                    Err(e) => {
                        let reply = Message::new(frame.id, msg::ERROR)
                            .with("code", "400")
                            .with("message", e.to_string());
                        respond(&mut ws, reply).await?;
                    }
                },
                msg::STATUS => {
                    let jobs = sched.store.list().unwrap_or_default();
                    let active = jobs.iter().filter(|j| j.state == JobState::Active).count();
                    let queued = jobs
                        .iter()
                        .filter(|j| {
                            matches!(
                                j.state,
                                JobState::Queued | JobState::Scheduled | JobState::Paused
                            )
                        })
                        .count();
                    let paused_quota = jobs.iter().filter(|j| j.quota_paused).count();
                    let snap = sched.quota.snapshot();
                    let reply = Message::new(frame.id, msg::STATUS_ACK)
                        .with("total", jobs.len().to_string())
                        .with("active", active.to_string())
                        .with("queued", queued.to_string())
                        .with("slots", sched.available_slots().to_string())
                        .with("quota_used", snap.used.to_string())
                        .with("quota_total", snap.limit.unwrap_or(0).to_string())
                        .with("quota_reset", snap.resets_in_secs.unwrap_or(0).to_string())
                        .with("quota_paused", paused_quota.to_string());
                    respond(&mut ws, reply).await?;
                }
                msg::BYE => break,
                other => {
                    let reply = Message::new(frame.id, msg::ERROR)
                        .with("code", "404")
                        .with("message", format!("unknown opcode {other}"));
                    respond(&mut ws, reply).await?;
                }
            }
        }
    }

    tracing::info!(%peer, "extension disconnected");
    Ok(())
}

struct HttpRequest {
    method: String,
    path: String,
    #[allow(dead_code)]
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

async fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest> {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];
    let header_end;
    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Err(RiptideError::Protocol(
                "connection closed before headers".into(),
            ));
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
        if buf.len() > 64 * 1024 {
            return Err(RiptideError::Protocol("request headers too large".into()));
        }
    }

    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();

    let mut headers = Vec::new();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim().to_string();
            if k == "content-length" {
                content_length = v.parse().unwrap_or(0);
            }
            headers.push((k, v));
        }
    }

    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_length);

    Ok(HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    Ok(())
}

async fn handle_http(stream: &mut TcpStream, sched: Arc<Scheduler>) -> Result<()> {
    let req = read_http_request(stream).await?;
    let path = req.path.split('?').next().unwrap_or("/").to_string();

    match (req.method.as_str(), path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => {
            write_response(
                stream,
                200,
                "OK",
                "text/html; charset=utf-8",
                INDEX_HTML.as_bytes(),
            )
            .await
        }
        ("GET", "/health") => write_response(stream, 200, "OK", "text/plain", b"ok").await,
        ("GET", "/api/quota") => {
            let snap = sched.quota.snapshot();
            let json = serde_json::to_vec_pretty(&snap).unwrap_or_else(|_| b"{}".to_vec());
            write_response(stream, 200, "OK", "application/json", &json).await
        }
        ("GET", "/api/jobs") => {
            let jobs = sched.store.list().unwrap_or_default();
            let json = serde_json::to_vec_pretty(&jobs).unwrap_or_else(|_| b"[]".to_vec());
            write_response(stream, 200, "OK", "application/json", &json).await
        }
        ("GET", p) if p.starts_with("/api/jobs/") => {
            let id = p.trim_start_matches("/api/jobs/");
            match sched.store.load(id) {
                Ok(job) => {
                    let json = serde_json::to_vec_pretty(&job).unwrap_or_default();
                    write_response(stream, 200, "OK", "application/json", &json).await
                }
                Err(_) => {
                    write_response(stream, 404, "Not Found", "text/plain", b"no such job").await
                }
            }
        }
        ("GET", "/api/projects") => {
            let projects = sched.projects.list().unwrap_or_default();
            let now = crate::util::now_ms();
            // The stored document plus the two derived strings, so a client does not have to
            // re-implement "is it due" to render a page.
            let view: Vec<serde_json::Value> = projects
                .iter()
                .map(|p| {
                    let mut v = serde_json::to_value(p).unwrap_or_default();
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert(
                            "next_run_in".into(),
                            serde_json::Value::String(p.schedule_summary(now)),
                        );
                        obj.insert(
                            "last_run".into(),
                            serde_json::Value::String(p.last_run_summary(now)),
                        );
                    }
                    v
                })
                .collect();
            let json = serde_json::to_vec_pretty(&view).unwrap_or_else(|_| b"[]".to_vec());
            write_response(stream, 200, "OK", "application/json", &json).await
        }
        ("POST", p) if p.starts_with("/api/projects/") => {
            // `POST /api/projects/<name>/run` runs one now, whatever its schedule says.
            let rest = p.trim_start_matches("/api/projects/");
            let Some((name, action)) = rest.rsplit_once('/') else {
                return write_response(
                    stream,
                    400,
                    "Bad Request",
                    "text/plain",
                    b"expected /api/projects/<name>/run",
                )
                .await;
            };
            if action != "run" {
                return write_response(stream, 404, "Not Found", "text/plain", b"unknown action")
                    .await;
            }
            let mut project = match sched.projects.load(name) {
                Ok(p) => p,
                Err(_) => {
                    return write_response(
                        stream,
                        404,
                        "Not Found",
                        "text/plain",
                        b"no such project",
                    )
                    .await;
                }
            };
            let runner = ProjectRunner::with_limits(
                sched.cfg.clone(),
                sched.global.clone(),
                sched.servers.clone(),
                sched.quota.clone(),
            );
            let outcome = runner.run(&mut project).await;
            let saved = sched.projects.save(&project);
            let body = serde_json::to_vec(&serde_json::json!({
                "name": project.name,
                "outcome": outcome.as_str(),
                "error": project.last_error,
                "saved": saved.is_ok(),
            }))
            .unwrap_or_default();
            let status = if outcome.is_failure() { 502 } else { 200 };
            write_response(stream, status, "OK", "application/json", &body).await
        }
        ("POST", "/api/jobs") => {
            let parsed: std::result::Result<serde_json::Value, _> =
                serde_json::from_slice(&req.body);
            let Ok(v) = parsed else {
                return write_response(
                    stream,
                    400,
                    "Bad Request",
                    "text/plain",
                    b"body must be JSON",
                )
                .await;
            };
            let Some(url) = v.get("url").and_then(|u| u.as_str()) else {
                return write_response(stream, 400, "Bad Request", "text/plain", b"missing 'url'")
                    .await;
            };
            let m = message_from_json(url, &v);
            match job_from_message(&sched.cfg, &m) {
                Ok(job) => {
                    let id = job.id.clone();
                    let this = sched.clone();
                    let handle = this.spawn(job);
                    tokio::spawn(async move {
                        let _ = handle.await;
                    });
                    let body =
                        serde_json::to_vec(&serde_json::json!({ "id": id, "status": "queued" }))
                            .unwrap_or_default();
                    write_response(stream, 201, "Created", "application/json", &body).await
                }
                Err(e) => {
                    write_response(
                        stream,
                        400,
                        "Bad Request",
                        "text/plain",
                        e.to_string().as_bytes(),
                    )
                    .await
                }
            }
        }
        _ => write_response(stream, 404, "Not Found", "text/plain", b"unknown endpoint").await,
    }
}

fn message_from_json(url: &str, v: &serde_json::Value) -> Message {
    let mut m = Message::new(0, msg::ADD).with("url", url);
    for (json_key, msg_key) in [
        ("output", "output"),
        ("connections", "connections"),
        ("quality", "quality"),
        ("kind", "kind"),
        ("referer", "referer"),
        ("cookie", "cookie"),
        ("user_agent", "user_agent"),
        ("proxy", "proxy"),
        ("auth", "auth"),
        ("speed_limit", "speed_limit"),
        ("checksum", "checksum"),
    ] {
        if let Some(v) = v.get(json_key) {
            let s = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            m = m.with(msg_key, s);
        }
    }
    m
}

const INDEX_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Riptide</title>
<style>
 body{font:14px/1.5 system-ui,sans-serif;margin:2rem auto;max-width:52rem;padding:0 1rem;color:#111}
 h1{font-size:1.4rem} code{background:#f4f4f5;padding:.1rem .3rem;border-radius:3px}
 input,button{font:inherit;padding:.4rem .6rem}
 li{margin:.25rem 0}
 .muted{color:#666}
</style></head>
<body>
<h1>Riptide</h1>
<p class="muted">Local download daemon. The browser extension speaks the
<code>MSG#</code> protocol over the <code>plugin.v3.internetdownloadmanager.com</code>
WebSocket subprotocol on this port. This page is the plain-HTTP control surface.</p>

<h2>Add a download</h2>
<p><input id="url" size="60" placeholder="https://host/file.iso">
<button onclick="add()">Queue</button></p>
<pre id="out"></pre>

<h2>Jobs</h2>
<p id="quota" class="muted"></p>
<ul id="jobs"></ul>

<h2>Projects</h2>
<p class="muted">Recurring work: a file kept in sync with a URL, a site re-crawled on a
schedule, or an offline mirror of a site and everything it needs to render. Created with
<code>riptide project add</code>; the daemon runs them when due.</p>
<ul id="projects"></ul>

<script>
async function add(){
  const url=document.getElementById('url').value.trim();
  if(!url) return;
  const r=await fetch('/api/jobs',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({url})});
  document.getElementById('out').textContent=await r.text();
  refresh();
}
async function refresh(){
  const jobs=await (await fetch('/api/jobs')).json();
  document.getElementById('jobs').innerHTML=jobs.map(j=>{
    const p=j.total?Math.round(j.downloaded/j.total*100):0;
    return `<li><b>${j.output.split('/').pop()}</b> — ${j.state}
      ${j.total?`(${p}% of ${j.total})`:''}<br><span class="muted">${j.url}</span></li>`;
  }).join('');
  const q=await (await fetch('/api/quota')).json();
  document.getElementById('quota').textContent = q.limit
    ? `Download quota: ${fmtBytes(q.used)} of ${fmtBytes(q.limit)} used this window`+
      (q.resets_in_secs!=null?`, refills in ${q.resets_in_secs}s`:'')+'.'
    : 'No download quota set.';

  const projects=await (await fetch('/api/projects')).json();
  document.getElementById('projects').innerHTML = projects.length ? projects.map(pr=>
    `<li><b>${pr.name}</b> — ${pr.kind}${pr.archive?' (archive)':''}, next ${pr.next_run_in}
      ${pr.enabled?'':'<span class="muted">(disabled)</span>'}
      <button onclick="runProject('${pr.name}')">Run now</button><br>
      <span class="muted">${pr.output} · last ${pr.last_run}${pr.last_found!=null?` · ${pr.last_found} file(s)`:''}</span></li>`
  ).join('') : '<li class="muted">No projects.</li>';
}
async function runProject(name){
  const r=await fetch(`/api/projects/${encodeURIComponent(name)}/run`,{method:'POST'});
  document.getElementById('out').textContent=await r.text();
  refresh();
}
function fmtBytes(n){
  const u=['B','KB','MB','GB','TB'];let i=0;
  while(n>=1024&&i<u.length-1){n/=1024;i++;}
  return (i?n.toFixed(2):n)+' '+u[i];
}
refresh();setInterval(refresh,1000);
</script>
</body></html>
"#;

async fn handle_conn(mut stream: TcpStream, peer: std::net::SocketAddr, sched: Arc<Scheduler>) {
    // Peek instead of read, so a WebSocket upgrade still sees an intact handshake.
    let mut peek = [0u8; 2048];
    let n = match stream.peek(&mut peek).await {
        Ok(n) => n,
        Err(e) => {
            tracing::debug!(%peer, error = %e, "peek failed");
            return;
        }
    };
    let head = String::from_utf8_lossy(&peek[..n]).to_ascii_lowercase();
    let is_ws = head.contains("upgrade: websocket") || head.contains("upgrade:websocket");

    let result = if is_ws {
        handle_ws(&mut stream, sched, peer).await
    } else {
        handle_http(&mut stream, sched).await
    };
    if let Err(e) = result {
        tracing::debug!(%peer, error = %e, "connection error");
    }
}

/// Bind the daemon socket and prepare its scheduler, without accepting connections yet.
///
/// Splitting binding from serving lets tests bind port 0 and learn the real address.
pub async fn bind(cfg: &Config) -> Result<(TcpListener, Arc<Scheduler>)> {
    let sched = Arc::new(Scheduler::new(cfg.clone())?);
    let listener = TcpListener::bind(&cfg.daemon_addr)
        .await
        .map_err(|e| RiptideError::other(format!("cannot bind {}: {e}", cfg.daemon_addr)))?;
    Ok((listener, sched))
}

/// Accept connections on an already-bound listener until the process stops.
pub async fn serve_on(listener: TcpListener, sched: Arc<Scheduler>) -> Result<()> {
    let addr = listener.local_addr()?;
    tracing::info!(
        %addr,
        protocol = WS_PROTOCOL,
        "daemon listening — browser extension endpoint and JSON API"
    );
    eprintln!("Riptide daemon listening on http://{addr}  (WebSocket subprotocol {WS_PROTOCOL})");

    tokio::spawn(project_ticker(sched.clone()));

    loop {
        let (stream, peer) = listener.accept().await?;
        let sched = sched.clone();
        tokio::spawn(async move { handle_conn(stream, peer, sched).await });
    }
}

/// Run due projects for as long as the daemon lives.
///
/// The runner shares the daemon's limiters, so a project competes for the same bandwidth
/// budget as an interactive download instead of doubling it.
async fn project_ticker(sched: Arc<Scheduler>) {
    let runner = ProjectRunner::with_limits(
        sched.cfg.clone(),
        sched.global.clone(),
        sched.servers.clone(),
        sched.quota.clone(),
    );
    let mut tick = tokio::time::interval(Duration::from_secs(PROJECT_TICK_SECS));
    // The first tick is immediate, which is what we want: a project whose time passed while
    // the daemon was down should run soon after it comes back, not a tick later.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        for (name, outcome) in sched.run_due_projects(&runner).await {
            if outcome.is_failure() {
                // The detail is on the project; this is the line that makes it visible.
                tracing::warn!(project = %name, outcome = outcome.as_str(), "project run failed");
            } else {
                tracing::info!(project = %name, outcome = outcome.as_str(), "project run finished");
            }
        }
    }
}

/// Run the daemon until the process is stopped.
pub async fn serve(cfg: Config) -> Result<()> {
    let (listener, sched) = bind(&cfg).await?;
    serve_on(listener, sched).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_checksum_survives_the_json_api_and_a_bad_one_is_refused() {
        let cfg = crate::config::Config::default();
        let good =
            crate::checksum::Checksum::of_bytes(crate::checksum::Algorithm::Sha256, b"payload");
        let v = serde_json::json!({ "url": "https://h/a.bin", "checksum": good.spec() });
        let m = message_from_json(v["url"].as_str().unwrap(), &v);
        let job = job_from_message(&cfg, &m).unwrap();
        assert_eq!(job.checksum.as_ref(), Some(&good));

        let bad = serde_json::json!({ "url": "https://h/a.bin", "checksum": "not-a-digest" });
        let m = message_from_json(bad["url"].as_str().unwrap(), &bad);
        assert!(job_from_message(&cfg, &m).is_err());
    }

    #[test]
    fn frames_round_trip() {
        let m = Message::new(7, msg::ADD)
            .with("url", "https://host/a b.iso")
            .with("cookie", "a=1;b=2")
            .with("user_agent", "Mozilla/5.0");
        let text = encode_frame(&m);
        assert!(text.starts_with("MSG#7#3#0#0,"));
        assert!(text.ends_with(';'));

        let (back, used) = parse_frame(&text).unwrap();
        assert_eq!(used, text.len());
        assert_eq!(back, m);
    }

    #[test]
    fn delimiters_inside_values_survive() {
        let m = Message::new(1, msg::ADD).with("cookie", "a=1;b=2,c=3");
        let text = encode_frame(&m);
        // The framing characters must not appear raw inside the value.
        let body = text.strip_prefix("MSG#1#3#0#0,").unwrap();
        assert!(body.contains('%'));
        let (back, _) = parse_frame(&text).unwrap();
        assert_eq!(back.get("cookie"), Some("a=1;b=2,c=3"));
    }

    /// The browser extension encodes values with `encodeURIComponent`. These are its exact
    /// outputs (checked against Node), so the two implementations must agree byte for byte.
    #[test]
    fn values_encoded_the_way_encodeuricomponent_does_are_decoded_verbatim() {
        let cases: [(&str, &str, &str); 5] = [
            (
                "url",
                "https%3A%2F%2Fh%2Fa%20b%3Fx%3D1%26y%3D2%2Cz%3Bw%23f%25g",
                "https://h/a b?x=1&y=2,z;w#f%g",
            ),
            (
                "referer",
                "https%3A%2F%2Fh%2Fp%3Fq%3Da%3Db%26r%3Dc",
                "https://h/p?q=a=b&r=c",
            ),
            ("cookie", "a%3D1%3B%20b%3D2%3B%20c%3D3", "a=1; b=2; c=3"),
            (
                "output",
                "%2Fhome%2Fme%2FMy%20Files%2Fx.iso",
                "/home/me/My Files/x.iso",
            ),
            (
                "headers_extra",
                "X-Api-Key%3A%20a%2Cb%3Bc%3Dd%0AX-Other%3A%201",
                "X-Api-Key: a,b;c=d\nX-Other: 1",
            ),
        ];
        for (key, encoded, want) in cases {
            let frame = format!("MSG#3#3#0#0,{key}={encoded};");
            let (m, used) = parse_frame(&frame).expect(&frame);
            assert_eq!(used, frame.len(), "{frame}");
            assert_eq!(m.get(key), Some(want), "{frame}");
        }
    }

    #[test]
    fn the_shipped_sample_frame_parses() {
        // A literal sample found inside IDMMsgHost.exe.
        let (m, used) = parse_frame("MSG#0#17#1#0:9:;").unwrap();
        assert_eq!(m.id, 0);
        assert_eq!(m.mtype, 17);
        assert_eq!(m.a, 1);
        assert_eq!(used, "MSG#0#17#1#0:9:;".len());
    }

    #[test]
    fn partial_frames_are_held_back() {
        let full = encode_frame(&Message::new(1, msg::HELLO));
        let partial = &full[..full.len() - 1];

        let (frames, tail) = drain_frames(partial);
        assert!(frames.is_empty());
        assert_eq!(tail, partial);

        // Completing the partial frame from the next message yields exactly one frame.
        let (frames2, tail2) = drain_frames(&format!("{full}{partial}"));
        assert_eq!(frames2.len(), 1);
        assert_eq!(tail2, partial, "the incomplete trailing frame is preserved");
    }

    #[test]
    fn several_frames_in_one_message() {
        let a = encode_frame(&Message::new(1, msg::PING));
        let b = encode_frame(&Message::new(2, msg::STATUS));
        let (frames, tail) = drain_frames(&format!("{a}{b}"));
        assert_eq!(frames.len(), 2);
        assert!(tail.is_empty());
    }

    #[test]
    fn leading_garbage_is_discarded() {
        let good = encode_frame(&Message::new(3, msg::PING));
        let (frames, tail) = drain_frames(&format!("garbage{good}"));
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].id, 3);
        assert!(tail.is_empty());
    }

    #[test]
    fn add_message_becomes_a_job() {
        let cfg = Config::default();
        let m = Message::new(1, msg::ADD)
            .with("url", "https://host/movie.mp4")
            .with("output", "/tmp/movie.mp4")
            .with("cookie", "session=abc")
            .with("referer", "https://host/page")
            .with("user_agent", "TestAgent/1")
            .with("connections", "4")
            .with("auth", "bob:hunter2");
        let job = job_from_message(&cfg, &m).unwrap();
        assert_eq!(job.url, "https://host/movie.mp4");
        assert_eq!(job.output, std::path::PathBuf::from("/tmp/movie.mp4"));
        assert!(!job.output_provisional);
        assert_eq!(job.connections, 4);
        assert_eq!(job.ctx.cookie.as_deref(), Some("session=abc"));
        assert_eq!(job.ctx.referer.as_deref(), Some("https://host/page"));
        assert_eq!(job.ctx.user_agent.as_deref(), Some("TestAgent/1"));
        assert!(job.ctx.auth.is_some());
    }

    #[test]
    fn a_page_cannot_ask_for_unbounded_connections() {
        let cfg = Config::default();
        let ask = |n: &str| {
            let m = Message::new(1, msg::ADD)
                .with("url", "https://host/movie.mp4")
                .with("connections", n);
            job_from_message(&cfg, &m).unwrap().connections
        };
        assert_eq!(ask("4"), 4);
        assert_eq!(ask("999"), MAX_CONNECTIONS_PER_FILE);
        // Nonsense and zero fall back to something usable rather than failing.
        assert_eq!(ask("0"), 1);
        assert_eq!(ask("many"), cfg.connections);
    }

    #[test]
    fn add_without_url_is_rejected() {
        let cfg = Config::default();
        let m = Message::new(1, msg::ADD);
        assert!(job_from_message(&cfg, &m).is_err());
    }

    #[test]
    fn provisional_output_when_no_path_given() {
        let cfg = Config::default();
        let m = Message::new(1, msg::ADD).with("url", "https://host/x");
        let job = job_from_message(&cfg, &m).unwrap();
        assert!(
            job.output_provisional,
            "the engine should name it from the response"
        );
    }

    #[test]
    fn manifest_kinds_are_recognised() {
        let cfg = Config::default();
        for (kind, url, want) in [
            ("hls", "https://h/x", JobKind::Hls),
            ("dash", "https://h/x", JobKind::Dash),
            ("ftp", "https://h/x", JobKind::Ftp),
            ("http", "https://h/x.m3u8", JobKind::Http),
            // No explicit kind, so the URL decides.
            ("", "https://h/x", JobKind::Http),
            ("", "https://h/live/master.m3u8", JobKind::Hls),
            ("", "https://h/dash/manifest.mpd", JobKind::Dash),
            ("", "ftp://h/pub/a.iso", JobKind::Ftp),
        ] {
            let m = Message::new(1, msg::ADD)
                .with("url", url)
                .with("kind", kind);
            assert_eq!(
                job_from_message(&cfg, &m).unwrap().kind,
                want,
                "{kind} {url}"
            );
        }
    }

    #[test]
    fn http_api_json_maps_to_a_message() {
        let v = serde_json::json!({
            "url": "https://h/f",
            "output": "/tmp/f",
            "connections": 16,
            "cookie": "a=b",
            "proxy": "socks5://127.0.0.1:9150"
        });
        let m = message_from_json("https://h/f", &v);
        let cfg = Config::default();
        let job = job_from_message(&cfg, &m).unwrap();
        assert_eq!(job.connections, 16);
        assert_eq!(job.ctx.cookie.as_deref(), Some("a=b"));
        assert_eq!(job.ctx.proxy.as_deref(), Some("socks5://127.0.0.1:9150"));
        // Numbers arriving as JSON numbers must still work.
        assert_eq!(job.output, std::path::PathBuf::from("/tmp/f"));
    }
}
