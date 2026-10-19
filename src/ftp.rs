//! FTP client.
//!
//! IDM's engine strings (`Cannot open ftp data socket`, `Cannot find IP address for data
//! socket in reply on PASV command.`) show it speaks FTP directly over its own socket
//! loop. Riptide does the same over tokio, and implements resume through `REST`, which is
//! the FTP equivalent of an HTTP byte range.
//!
//! Explicit/implicit TLS (`ftps://`) is **not** implemented — see `docs/FEATURE-MAP.md`.

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::engine::Engine;
use crate::error::{Result, RiptideError};
use crate::storage::Sink;

/// A parsed `ftp://` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtpUrl {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub pass: String,
    /// Path with a leading slash, percent-decoded, as FTP expects.
    pub path: String,
}

pub const DEFAULT_FTP_PORT: u16 = 21;

/// Parse an `ftp://` URL, applying conventional defaults for missing parts.
pub fn parse_ftp_url(raw: &str) -> Result<FtpUrl> {
    let u = url::Url::parse(raw).map_err(RiptideError::Url)?;
    match u.scheme() {
        "ftp" => {}
        "ftps" => {
            return Err(RiptideError::Unsupported(
                "FTPS (FTP over TLS) is not implemented; use plain ftp:// for now".into(),
            ))
        }
        other => return Err(RiptideError::other(format!("{other}:// is not an FTP URL"))),
    }
    let host = u
        .host_str()
        .ok_or_else(|| RiptideError::other("FTP URL has no host"))?
        .to_string();
    let port = u.port().unwrap_or(DEFAULT_FTP_PORT);

    let user = if u.username().is_empty() {
        "anonymous".to_string()
    } else {
        percent_encoding::percent_decode_str(u.username())
            .decode_utf8_lossy()
            .to_string()
    };
    let pass = u
        .password()
        .map(|p| {
            percent_encoding::percent_decode_str(p)
                .decode_utf8_lossy()
                .to_string()
        })
        // The conventional anonymous password is an email address.
        .unwrap_or_else(|| {
            if u.username().is_empty() {
                "anonymous@".to_string()
            } else {
                String::new()
            }
        });

    let path = percent_encoding::percent_decode_str(u.path())
        .decode_utf8_lossy()
        .to_string();
    let path = if path.is_empty() {
        "/".to_string()
    } else {
        path
    };

    Ok(FtpUrl {
        host,
        port,
        user,
        pass,
        path,
    })
}

/// Filename implied by an FTP path.
pub fn filename_of(path: &str) -> Option<String> {
    let last = path.trim_end_matches('/').rsplit('/').next()?;
    if last.is_empty() {
        None
    } else {
        Some(crate::util::sanitize_filename(last))
    }
}

/// Parse a `227 Entering Passive Mode (h1,h2,h3,h4,p1,p2)` reply.
pub fn parse_pasv(reply: &str) -> Result<String> {
    let open = reply
        .find('(')
        .ok_or_else(|| RiptideError::Protocol("PASV reply has no address".into()))?;
    let close = reply
        .find(')')
        .ok_or_else(|| RiptideError::Protocol("PASV reply has no closing paren".into()))?;
    let inner = &reply[open + 1..close];
    let parts: Vec<u16> = inner
        .split(',')
        .filter_map(|p| p.trim().parse::<u16>().ok())
        .collect();
    if parts.len() != 6 {
        return Err(RiptideError::Protocol(format!(
            "PASV reply did not contain six numbers: {inner:?}"
        )));
    }
    let addr = format!(
        "{}.{}.{}.{}:{}",
        parts[0],
        parts[1],
        parts[2],
        parts[3],
        parts[4] * 256 + parts[5]
    );
    Ok(addr)
}

/// Parse `213 12345` (SIZE) or `213-...` replies into the first number found.
pub fn parse_size_reply(reply: &str) -> Option<u64> {
    reply.lines().find_map(|l| {
        l.split_whitespace()
            .nth(1)
            .and_then(|n| n.parse::<u64>().ok())
    })
}

/// Parse the offset out of the argument to `REST` / the reply to `REST`.
pub fn parse_rest_reply(reply: &str) -> Option<u64> {
    reply
        .split_whitespace()
        .filter_map(|t| t.parse::<u64>().ok())
        .next_back()
}

struct Control {
    reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: tokio::net::tcp::OwnedWriteHalf,
}

impl Control {
    async fn connect(host: &str, port: u16, timeout: Duration) -> Result<Self> {
        let stream = tokio::time::timeout(timeout, TcpStream::connect((host, port)))
            .await
            .map_err(|_| RiptideError::other(format!("timed out connecting to {host}:{port}")))??;
        stream.set_nodelay(true).ok();
        let (r, w) = stream.into_split();
        Ok(Control {
            reader: BufReader::new(r),
            writer: w,
        })
    }

    /// Read one (possibly multi-line) FTP reply: `NNN-...` lines continue, `NNN ...` ends.
    async fn read_reply(&mut self) -> Result<(u16, String)> {
        let mut text = String::new();
        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).await?;
            if n == 0 {
                return Err(RiptideError::Protocol(
                    "FTP control connection closed unexpectedly".into(),
                ));
            }
            let t = line.trim_end_matches(['\r', '\n']).to_string();
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&t);

            if t.len() >= 4 {
                let code: u16 = t[..3].parse().unwrap_or(0);
                if t.as_bytes()[3] == b' ' {
                    return Ok((code, text));
                }
            } else if t.len() == 3 {
                let code: u16 = t.parse().unwrap_or(0);
                return Ok((code, text));
            }
        }
    }

    async fn cmd(&mut self, line: &str) -> Result<(u16, String)> {
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.write_all(b"\r\n").await?;
        self.writer.flush().await?;
        self.read_reply().await
    }

    async fn quit(&mut self) {
        let _ = self.cmd("QUIT").await;
    }
}

/// Download an `ftp://` URL, resuming with `REST` when a partial file already exists.
pub async fn download(engine: &mut Engine) -> Result<()> {
    let raw = engine
        .job()
        .resolved_url
        .clone()
        .unwrap_or_else(|| engine.job().url.clone());
    let target = parse_ftp_url(&raw)?;

    let connect_timeout = Duration::from_secs(engine.cfg.connect_timeout_secs.max(1));
    let read_timeout = Duration::from_secs(engine.cfg.read_timeout_secs.max(1));

    // Resolve a provisional output name from the FTP path.
    if engine.job().output_provisional {
        if let Some(name) = filename_of(&target.path) {
            engine.job_mut().output = engine.cfg.resolve_output(None, &name, None);
            engine.job_mut().output_provisional = false;
        }
    }

    let mut ctrl = Control::connect(&target.host, target.port, connect_timeout).await?;
    let (code, banner) = ctrl.read_reply().await?;
    if code != 220 {
        return Err(RiptideError::Protocol(format!(
            "FTP server refused the connection: {banner}"
        )));
    }

    let (code, msg) = ctrl.cmd(&format!("USER {}", target.user)).await?;
    match code {
        230 => {}
        331 => {
            let (code, msg) = ctrl.cmd(&format!("PASS {}", target.pass)).await?;
            if code != 230 {
                return Err(RiptideError::AuthFailed(format!(
                    "FTP login failed ({}): {msg}",
                    code
                )));
            }
        }
        _ => {
            return Err(RiptideError::AuthFailed(format!(
                "FTP USER rejected ({}): {msg}",
                code
            )))
        }
    }

    // Binary mode is mandatory: ASCII mode would rewrite line endings and corrupt the file.
    let (code, msg) = ctrl.cmd("TYPE I").await?;
    if code != 200 {
        return Err(RiptideError::Protocol(format!(
            "FTP TYPE I failed ({}): {msg}",
            code
        )));
    }

    // SIZE tells us whether resume is meaningful.
    let (size_code, size_msg) = ctrl.cmd(&format!("SIZE {}", target.path)).await?;
    let total = if size_code == 213 {
        parse_size_reply(&size_msg)
    } else {
        None
    };
    engine.job_mut().total = total;
    engine.job_mut().resumable = total.is_some();

    let part = engine.job().part_path();
    let existing = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let offset = match total {
        Some(t) if existing > 0 && existing < t => existing,
        Some(t) if existing >= t && t > 0 => {
            // Already complete on disk.
            let sink = Sink::create(&part, Some(t))?;
            engine.verify_checksum(&part).await?;
            sink.finalize(&engine.job().output.clone())?;
            engine.job_mut().downloaded = t;
            return Ok(());
        }
        _ => 0,
    };

    if offset > 0 {
        let (code, msg) = ctrl.cmd(&format!("REST {offset}")).await?;
        if code != 350 {
            // Not fatal — some servers advertise SIZE but refuse REST. Restart instead.
            tracing::warn!(code, reply = %msg, "FTP REST refused; restarting the transfer");
            let sink = Sink::create(&part, None)?;
            sink.truncate_to(0)?;
        }
    }

    let data = open_data_connection(&mut ctrl, connect_timeout).await?;
    let (code, msg) = ctrl.cmd(&format!("RETR {}", target.path)).await?;
    if code != 150 && code != 125 {
        return Err(RiptideError::Protocol(format!(
            "FTP RETR refused ({}): {msg}",
            code
        )));
    }

    let sink = Sink::create(&part, total)?;
    let mut reader = data;
    let mut pos = offset;
    let mut buf = vec![0u8; 1 << 16];

    loop {
        if engine.is_cancelled() {
            ctrl.quit().await;
            return Err(RiptideError::Cancelled);
        }
        let n = match tokio::time::timeout(read_timeout, reader.read(&mut buf)).await {
            Err(_) => {
                ctrl.quit().await;
                return Err(RiptideError::other("FTP data connection timed out"));
            }
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => n,
            Ok(Err(e)) => {
                ctrl.quit().await;
                return Err(e.into());
            }
        };
        // Charge before writing, so a spent budget never results in unaccounted bytes
        // (and no partially-credited file) on disk.
        engine.charge_bytes(n as u64).await?;
        sink.write_at(bytes::Bytes::copy_from_slice(&buf[..n]), pos)
            .await?;
        pos += n as u64;
        engine.job_mut().downloaded = pos;
    }

    // 226 Transfer complete.
    match tokio::time::timeout(read_timeout, ctrl.read_reply()).await {
        Ok(Ok((code, msg))) => {
            if code != 226 && code != 250 {
                ctrl.quit().await;
                return Err(RiptideError::Protocol(format!(
                    "FTP transfer did not complete cleanly ({}): {msg}",
                    code
                )));
            }
        }
        _ => tracing::debug!("no completion reply from the FTP server"),
    }
    ctrl.quit().await;

    if let Some(t) = total {
        if pos != t {
            return Err(RiptideError::Protocol(format!(
                "FTP transfer ended early: got {pos} of {t} bytes"
            )));
        }
    } else {
        engine.job_mut().total = Some(pos);
    }
    engine.job_mut().downloaded = pos;

    sink.sync().await?;
    engine.verify_checksum(&part).await?;
    sink.finalize(&engine.job().output.clone())?;
    Ok(())
}

async fn open_data_connection(ctrl: &mut Control, timeout: Duration) -> Result<TcpStream> {
    let (code, msg) = ctrl.cmd("PASV").await?;
    if code != 227 {
        return Err(RiptideError::Protocol(format!(
            "FTP PASV failed ({}): {msg}",
            code
        )));
    }
    let addr = parse_pasv(&msg)?;
    let stream = tokio::time::timeout(timeout, TcpStream::connect(&addr))
        .await
        .map_err(|_| {
            RiptideError::other(format!("timed out connecting to FTP data port {addr}"))
        })??;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ftp_urls() {
        let u = parse_ftp_url("ftp://ftp.example.com/pub/file.iso").unwrap();
        assert_eq!(u.host, "ftp.example.com");
        assert_eq!(u.port, 21);
        assert_eq!(u.user, "anonymous");
        assert_eq!(u.path, "/pub/file.iso");

        let u = parse_ftp_url("ftp://bob:s3cret@mirror.test:2121/linux/distro.iso").unwrap();
        assert_eq!(u.user, "bob");
        assert_eq!(u.pass, "s3cret");
        assert_eq!(u.port, 2121);
        assert_eq!(u.path, "/linux/distro.iso");
    }

    #[test]
    fn percent_encoded_credentials_are_decoded() {
        let u = parse_ftp_url("ftp://user%40corp.com:p%40ss@host/x").unwrap();
        assert_eq!(u.user, "user@corp.com");
        assert_eq!(u.pass, "p@ss");
    }

    #[test]
    fn ftps_is_explicitly_unsupported() {
        match parse_ftp_url("ftps://host/x") {
            Err(RiptideError::Unsupported(_)) => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn rejects_non_ftp_schemes() {
        assert!(parse_ftp_url("http://host/x").is_err());
    }

    #[test]
    fn filenames_from_paths() {
        assert_eq!(filename_of("/a/b/file.iso").as_deref(), Some("file.iso"));
        assert_eq!(filename_of("/a/b/").as_deref(), Some("b"));
        assert_eq!(filename_of("/"), None);
    }

    #[test]
    fn pasv_address_assembly() {
        let reply = "227 Entering Passive Mode (192,168,1,10,195,80)";
        assert_eq!(parse_pasv(reply).unwrap(), "192.168.1.10:50000");
        assert!(parse_pasv("227 no parens here").is_err());
    }

    #[test]
    fn size_and_rest_replies() {
        assert_eq!(parse_size_reply("213 1048576"), Some(1048576));
        assert_eq!(parse_size_reply("550 Could not get file size"), None);
        assert_eq!(parse_rest_reply("350 Restarting at 1024"), Some(1024));
    }
}
