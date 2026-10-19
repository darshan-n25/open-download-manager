//! End-to-end `Negotiate` (SPNEGO) authentication.
//!
//! There is no Windows/IIS server and no Kerberos realm in this environment, so the
//! *server* side is implemented here from RFC 4559 and MS-NLMP: it answers a bare
//! `WWW-Authenticate: Negotiate` with no token, unwraps the client's SPNEGO token,
//! replies with an NTLMSSP CHALLENGE inside a `NegTokenResp`, and verifies the
//! AUTHENTICATE message's NTProofStr against the challenge it issued before serving
//! the file.
//!
//! That is enough to prove the client half: the wrapping is accepted by a server
//! that only reads it as SPNEGO, the challenge is unwrapped from the server's
//! reply, and the proof is bound to the challenge the server chose. It is **not**
//! evidence about real IIS or Active Directory behaviour, and it says nothing
//! about the Kerberos path, which Riptide does not implement.

#![allow(clippy::field_reassign_with_default)]

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use riptide::auth::base64_encode;
use riptide::auth::spnego::{self, Mech};
use riptide::config::Config;
use riptide::engine::Engine;
use riptide::job::{Job, JobKind, JobState};
use riptide::scheduler::Scheduler;

const USER: &str = "alice";
const DOMAIN: &str = "CORP";
const PASSWORD: &str = "correct horse battery staple";
/// Credentials in the spelling the CLI accepts.
const CREDS: &str = "negotiate:CORP\\alice:correct horse battery staple";

/// How the mock server behaves, so one implementation covers the good path and the
/// two ways a Negotiate job can legitimately fail.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// A server that accepts NTLM inside SPNEGO — IIS's default provider order.
    Spnego,
    /// A server configured for Kerberos only: it answers every challenge with a
    /// Kerberos mechanism token.
    KerberosOnly,
    /// A server that only advertises bare NTLM, so a `negotiate:` job has nothing
    /// it can answer.
    BareNtlmOnly,
}

struct MockServer {
    addr: std::net::SocketAddr,
    mode: Mode,
    body: Vec<u8>,
    /// The challenge the server issued; a fixed value keeps the two handshake legs
    /// (which arrive on separate connections) comparable.
    challenge: [u8; 8],
    type1_seen: AtomicUsize,
    type3_seen: AtomicUsize,
    proofs_verified: AtomicUsize,
    proofs_rejected: AtomicUsize,
}

impl MockServer {
    async fn start(mode: Mode) -> Arc<MockServer> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut body = Vec::new();
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        while body.len() < 8192 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            body.extend_from_slice(&x.to_le_bytes());
        }
        let srv = Arc::new(MockServer {
            addr,
            mode,
            body,
            challenge: [0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38],
            type1_seen: AtomicUsize::new(0),
            type3_seen: AtomicUsize::new(0),
            proofs_verified: AtomicUsize::new(0),
            proofs_rejected: AtomicUsize::new(0),
        });

        let s = srv.clone();
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    break;
                };
                let s2 = s.clone();
                tokio::spawn(async move {
                    let mut sock = sock;
                    s2.serve(&mut sock).await;
                });
            }
        });
        srv
    }

    fn url(&self) -> String {
        format!("http://{}/file.bin", self.addr)
    }

    async fn serve(&self, sock: &mut TcpStream) {
        let Some((method, _path, auth)) = read_request(sock).await else {
            return;
        };
        let head_only = method.eq_ignore_ascii_case("HEAD");

        let Some(header) = auth else {
            // RFC 4559 4.1: a token-less challenge is how a server invites the
            // client to open the exchange.
            return self.challenge(sock, "Negotiate").await;
        };

        match self.mode {
            // Refuses to speak SPNEGO at all, and never offers a token.
            Mode::BareNtlmOnly => return self.challenge(sock, "NTLM").await,
            // Insists on Kerberos, and says so with a real Kerberos mechanism token.
            Mode::KerberosOnly => {
                return self
                    .challenge(
                        sock,
                        &format!("Negotiate {}", base64_encode(&kerberos_token())),
                    )
                    .await
            }
            Mode::Spnego => {}
        }

        // The good path: unwrap whatever the client sent and answer in kind.
        let Some(payload) = spnego::negotiate_payload(&header) else {
            return self.challenge(sock, "Negotiate").await;
        };
        let Some(token) = spnego::parse_server_token(&payload).and_then(|t| t.token) else {
            return self.challenge(sock, "Negotiate").await;
        };
        match ntlmssp_type(&token) {
            Some(1) => {
                // The client offered NTLM. Answer with a CHALLENGE wrapped in a
                // NegTokenResp, which is the shape RFC 4559 5 describes.
                self.type1_seen.fetch_add(1, Ordering::SeqCst);
                let type2 = self.type2();
                self.challenge(
                    sock,
                    &format!(
                        "Negotiate {}",
                        base64_encode(&spnego::response_token(&type2))
                    ),
                )
                .await
            }
            Some(3) => {
                self.type3_seen.fetch_add(1, Ordering::SeqCst);
                if self.verify_type3(&token) {
                    self.proofs_verified.fetch_add(1, Ordering::SeqCst);
                    self.ok(sock, head_only).await
                } else {
                    self.proofs_rejected.fetch_add(1, Ordering::SeqCst);
                    self.status(sock, "403 Forbidden", &[], head_only).await
                }
            }
            _ => self.challenge(sock, "Negotiate").await,
        }
    }

    async fn challenge(&self, sock: &mut TcpStream, scheme: &str) {
        self.status(
            sock,
            "401 Unauthorized",
            &[("WWW-Authenticate", scheme.to_string())],
            false,
        )
        .await;
    }

    async fn ok(&self, sock: &mut TcpStream, head_only: bool) {
        self.status(
            sock,
            "200 OK",
            &[("Content-Type", "application/octet-stream".to_string())],
            head_only,
        )
        .await;
    }

    async fn status(
        &self,
        sock: &mut TcpStream,
        status: &str,
        extra: &[(&str, String)],
        head_only: bool,
    ) {
        let mut h = format!("HTTP/1.1 {status}\r\n");
        for (k, v) in extra {
            h.push_str(&format!("{k}: {v}\r\n"));
        }
        h.push_str(&format!("Content-Length: {}\r\n", self.body.len()));
        h.push_str("Connection: close\r\n\r\n");
        let _ = sock.write_all(h.as_bytes()).await;
        if !head_only && status.starts_with("200") {
            let _ = sock.write_all(&self.body).await;
        }
        let _ = sock.flush().await;
    }

    /// An NTLMSSP CHALLENGE with an empty target name and a 4-byte AV terminator.
    fn type2(&self) -> Vec<u8> {
        const NEGOTIATE_UNICODE: u32 = 0x0000_0001;
        const NEGOTIATE_NTLM: u32 = 0x0000_0200;
        const NEGOTIATE_TARGET_INFO: u32 = 0x0080_0000;
        let target_info = [0u8; 4];

        let mut m = Vec::new();
        m.extend_from_slice(b"NTLMSSP\0");
        m.extend_from_slice(&2u32.to_le_bytes());
        // TargetName: empty, at offset 0.
        m.extend_from_slice(&0u16.to_le_bytes());
        m.extend_from_slice(&0u16.to_le_bytes());
        m.extend_from_slice(&0u32.to_le_bytes());
        m.extend_from_slice(
            &(NEGOTIATE_UNICODE | NEGOTIATE_NTLM | NEGOTIATE_TARGET_INFO).to_le_bytes(),
        );
        m.extend_from_slice(&self.challenge);
        m.extend_from_slice(&[0u8; 8]); // reserved
        m.extend_from_slice(&(target_info.len() as u16).to_le_bytes());
        m.extend_from_slice(&(target_info.len() as u16).to_le_bytes());
        m.extend_from_slice(&48u32.to_le_bytes());
        m.extend_from_slice(&target_info);
        m
    }

    /// Recompute the NTLMv2 proof the client should have produced, from the
    /// credentials this server is configured to accept.
    fn verify_type3(&self, m: &[u8]) -> bool {
        if m.len() < 64 || ntlmssp_type(m) != Some(3) {
            return false;
        }
        let nt_len = u16::from_le_bytes([m[20], m[21]]) as usize;
        let nt_off = u32::from_le_bytes([m[24], m[25], m[26], m[27]]) as usize;
        let Some(nt) = m.get(nt_off..nt_off + nt_len) else {
            return false;
        };
        if nt.len() < 16 {
            return false;
        }
        let (proof, blob) = nt.split_at(16);

        let mut input = Vec::with_capacity(8 + blob.len());
        input.extend_from_slice(&self.challenge);
        input.extend_from_slice(blob);

        let key = riptide::auth::ntlm::ntowfv2(USER, DOMAIN, PASSWORD);
        use hmac::{Hmac, Mac};
        let mut mac = Hmac::<md5::Md5>::new_from_slice(&key).expect("any key length");
        mac.update(&input);
        let expected = mac.finalize().into_bytes();
        proof == &expected[..]
    }
}

/// The NTLMSSP message type at offset 8, or `None` when this is not one.
fn ntlmssp_type(m: &[u8]) -> Option<u32> {
    if m.len() < 12 || &m[0..8] != b"NTLMSSP\0" {
        return None;
    }
    Some(u32::from_le_bytes([m[8], m[9], m[10], m[11]]))
}

/// A well-formed Kerberos GSS-API token: `[APPLICATION 0] { OID 1.2.840.113554.1.2.2, ... }`.
fn kerberos_token() -> Vec<u8> {
    let krb5_body = [0x6b, 0x12, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05];
    let mut inner = vec![
        0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x12, 0x01, 0x02, 0x02,
    ];
    inner.extend_from_slice(&krb5_body);
    let mut tok = vec![0x60, inner.len() as u8];
    tok.extend_from_slice(&inner);
    tok
}

async fn read_request(sock: &mut TcpStream) -> Option<(String, String, Option<String>)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    loop {
        let n = sock.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 64 * 1024 {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.lines();
    let request = lines.next()?;
    let mut parts = request.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();

    let mut auth = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("authorization") {
                auth = Some(v.trim().to_string());
            }
        }
    }
    Some((method, path, auth))
}

fn test_config(state_dir: &Path, download_dir: &Path) -> Config {
    let mut cfg = Config::default();
    cfg.state_dir = state_dir.to_path_buf();
    cfg.download_dir = download_dir.to_path_buf();
    cfg.connections = 2;
    cfg.min_chunk = 4096;
    cfg.retry_backoff_ms = 1;
    cfg.auto_categorize = false;
    cfg
}

fn authed_job(url: &str, output: &Path, creds: &str) -> Job {
    let mut job = Job::new(
        "test".into(),
        url.to_string(),
        output.to_path_buf(),
        JobKind::Http,
    );
    job.connections = 2;
    job.min_chunk = 4096;
    job.max_retries = 1;
    job.output_provisional = false;
    job.ctx.auth = riptide::auth::parse_auth_spec(creds);
    job
}

#[tokio::test]
async fn a_negotiate_job_completes_the_spnego_handshake_and_downloads() {
    let dir = tempfile::tempdir().unwrap();
    let srv = MockServer::start(Mode::Spnego).await;
    let out = dir.path().join("file.bin");

    let cfg = test_config(&dir.path().join("state"), &dir.path().join("dl"));
    let sched = Scheduler::new(cfg).unwrap();
    let job = sched
        .execute(authed_job(&srv.url(), &out, CREDS))
        .await
        .expect("the handshake should succeed");
    assert_eq!(job.state, JobState::Done);
    assert_eq!(std::fs::read(&out).unwrap(), srv.body, "bytes must match");

    // The server saw a real two-leg handshake, and the proof it verified was bound
    // to the challenge it issued — the assertion in the handler would have failed
    // otherwise.
    assert!(
        srv.type1_seen.load(Ordering::SeqCst) >= 1,
        "no NEGOTIATE seen"
    );
    assert!(
        srv.type3_seen.load(Ordering::SeqCst) >= 1,
        "no AUTHENTICATE seen"
    );
    assert!(srv.proofs_verified.load(Ordering::SeqCst) >= 1);
    assert_eq!(
        srv.proofs_rejected.load(Ordering::SeqCst),
        0,
        "every AUTHENTICATE must verify"
    );
}

#[tokio::test]
async fn a_negotiate_only_server_refuses_a_bare_ntlm_job_clearly() {
    // `ntlm:` and `negotiate:` share one code path, so this pins the separation:
    // bare NTLM against a server that only speaks Negotiate must fail with a
    // message about the missing challenge, not a panic or a partial file.
    let dir = tempfile::tempdir().unwrap();
    let srv = MockServer::start(Mode::Spnego).await;
    let out = dir.path().join("file.bin");

    let cfg = test_config(&dir.path().join("state"), &dir.path().join("dl"));
    let mut engine = Engine::new(cfg, authed_job(&srv.url(), &out, "ntlm:CORP\\alice:c")).unwrap();
    let err = engine.probe().await.expect_err("nothing to answer");
    assert!(
        err.to_string().contains("NTLM"),
        "the error must name the scheme that was missing: {err}"
    );
    assert!(!out.exists());
}

#[tokio::test]
async fn a_wrong_password_is_refused_by_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let srv = MockServer::start(Mode::Spnego).await;
    let out = dir.path().join("file.bin");

    let cfg = test_config(&dir.path().join("state"), &dir.path().join("dl"));
    let sched = Scheduler::new(cfg).unwrap();
    let bad = "negotiate:CORP\\alice:wrong password";
    let err = sched.execute(authed_job(&srv.url(), &out, bad)).await;
    assert!(err.is_err(), "a bad password must not download anything");
    assert_eq!(
        srv.proofs_verified.load(Ordering::SeqCst),
        0,
        "the server must not have accepted a proof"
    );
    assert!(srv.proofs_rejected.load(Ordering::SeqCst) >= 1);
    assert!(!out.exists(), "no file may be published");
}

#[tokio::test]
async fn a_kerberos_only_server_is_reported_in_those_words() {
    let dir = tempfile::tempdir().unwrap();
    let srv = MockServer::start(Mode::KerberosOnly).await;
    let out = dir.path().join("file.bin");

    let cfg = test_config(&dir.path().join("state"), &dir.path().join("dl"));
    let mut engine = Engine::new(cfg, authed_job(&srv.url(), &out, CREDS)).unwrap();
    let err = engine
        .probe()
        .await
        .expect_err("Kerberos cannot be answered");
    let msg = err.to_string();
    assert!(
        msg.contains("Kerberos"),
        "message must name the mechanism: {msg}"
    );
    assert!(
        msg.contains("NTLM"),
        "and must say what it does support: {msg}"
    );
    assert!(
        !msg.contains("unreadable"),
        "a Kerberos token is readable, it is just unusable: {msg}"
    );
}

#[tokio::test]
async fn kerberos_is_recognised_by_the_classifier_too() {
    // The engine's diagnosis rests on this, so pin it against the same bytes the
    // mock server sends.
    let tok = kerberos_token();
    let st = spnego::parse_server_token(&tok).expect("parses");
    assert_eq!(st.mech(), Some(Mech::Kerberos));
}

#[tokio::test]
async fn a_server_that_only_advertises_bare_ntlm_gives_a_clear_error() {
    let dir = tempfile::tempdir().unwrap();
    let srv = MockServer::start(Mode::BareNtlmOnly).await;
    let out = dir.path().join("file.bin");

    let cfg = test_config(&dir.path().join("state"), &dir.path().join("dl"));
    let mut engine = Engine::new(cfg, authed_job(&srv.url(), &out, CREDS)).unwrap();
    let err = engine.probe().await.expect_err("nothing to answer");
    let msg = err.to_string();
    assert!(
        msg.contains("Negotiate") || msg.contains("ntlm:"),
        "the error should point at the right knob: {msg}"
    );
}
