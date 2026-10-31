//! Serve a torrent's file over HTTP while it downloads.
//!
//! This is the piece that turns a download into something you can watch. The server
//! is a normal HTTP/1.1 file server — `HEAD`, `GET`, `Range` — with one difference:
//! when the range the client asked for is not on disk yet, it does not return 404 or
//! a short body. It waits for the pieces the player needs, and it tells the picker
//! where the player is so the swarm fetches *that* part of the file first.
//!
//! That is the same shape as uTorrent's streaming: a local endpoint with byte ranges,
//! backed by a sequential piece policy (libtorrent's `set_piece_deadline` and
//! `media_streamer`). No second player engine is involved; any client that speaks
//! `Range` — our own window, VLC, a browser — can play from it.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::bt::session::Session;
use crate::error::{OdmError, Result};

/// How long a request will wait for one piece before giving up. Generous: a slow
/// swarm is still a playable swarm, and the player will retry the range.
const PIECE_WAIT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
pub struct StreamInfo {
    /// `http://127.0.0.1:<port>/<token>/<file>`
    pub url: String,
    pub addr: SocketAddr,
    pub token: String,
    pub file: usize,
}

/// Start a localhost stream server for one file of the session's torrent.
pub async fn start(session: Arc<Session>, file: usize) -> Result<StreamInfo> {
    start_with_cancel(session, file, Arc::new(AtomicBool::new(false))).await
}

/// The same server, but stopping when `cancel` becomes true.
///
/// A desktop player opens a server per viewing and closes it when the window is
/// closed; without a way to stop, every play would leave a listener (and the
/// session it holds) behind for the lifetime of the program.
pub async fn start_with_cancel(
    session: Arc<Session>,
    file: usize,
    cancel: Arc<AtomicBool>,
) -> Result<StreamInfo> {
    if file >= session.torrent.files.len() {
        return Err(OdmError::other(format!(
            "the torrent has {} files, not {}",
            session.torrent.files.len(),
            file + 1
        )));
    }
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| OdmError::other(format!("stream listener: {e}")))?;
    let addr = listener
        .local_addr()
        .map_err(|e| OdmError::other(e.to_string()))?;
    let mut raw = [0u8; 8];
    rand::Rng::fill(&mut rand::thread_rng(), &mut raw);
    let token = hex::encode(raw);

    let url = format!("http://127.0.0.1:{}/{}/{}", addr.port(), token, file);
    let info = StreamInfo {
        url,
        addr,
        token: token.clone(),
        file,
    };

    tokio::spawn(accept_loop(listener, session, token, cancel));
    Ok(info)
}

async fn accept_loop(
    listener: TcpListener,
    session: Arc<Session>,
    token: String,
    cancel: Arc<AtomicBool>,
) {
    loop {
        // `accept` waits forever on an idle port, so the flag is checked on a timer
        // rather than only when the next client arrives.
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            _ = tokio::time::sleep(Duration::from_millis(400)) => {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                continue;
            }
        };
        let (stream, _) = match accepted {
            Ok(v) => v,
            Err(_) => return,
        };
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let session = session.clone();
        let token = token.clone();
        tokio::spawn(async move {
            let _ = serve(stream, session, token).await;
        });
    }
}

async fn serve(mut stream: TcpStream, session: Arc<Session>, token: String) -> Result<()> {
    stream.set_nodelay(true).ok();
    let (method, path, range) = match read_request(&mut stream).await {
        Ok(Some(req)) => req,
        _ => return Ok(()),
    };

    // The first path segment is the capability token, the second the file index.
    let mut parts = path.trim_start_matches('/').splitn(2, '/');
    let given = parts.next().unwrap_or("");
    if given != token {
        return respond(&mut stream, 403, "Forbidden", &[], b"forbidden").await;
    }
    let file: usize = parts
        .next()
        .and_then(|s| s.split('?').next())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if file >= session.torrent.files.len() {
        return respond(&mut stream, 404, "Not Found", &[], b"no such file").await;
    }

    let entry = &session.torrent.files[file];
    let total = entry.length;
    let (start, end) = match range {
        Some((a, b)) => {
            let start = a.unwrap_or_else(|| total.saturating_sub(b.unwrap_or(0)));
            let end = b
                .unwrap_or(total.saturating_sub(1))
                .min(total.saturating_sub(1));
            if start > end || start >= total {
                let headers = [("Content-Range", format!("bytes */{total}"))];
                return respond(&mut stream, 416, "Range Not Satisfiable", &headers, b"").await;
            }
            (start, end)
        }
        None => (0, total.saturating_sub(1)),
    };
    let len = end + 1 - start;
    let mime = mime_for(entry.path.last().map(String::as_str).unwrap_or(""));
    let content_range = format!("bytes {start}-{end}/{total}");

    let mut headers: Vec<(&str, String)> = vec![
        ("Content-Type", mime.to_string()),
        ("Accept-Ranges", "bytes".to_string()),
        ("Content-Length", len.to_string()),
        ("Cache-Control", "no-store".to_string()),
    ];
    if len != total || start != 0 {
        headers.push(("Content-Range", content_range));
    }

    let status = if range.is_some() { 206 } else { 200 };
    let reason = if range.is_some() {
        "Partial Content"
    } else {
        "OK"
    };
    write_head(&mut stream, status, reason, &headers).await?;
    if method == "HEAD" || len == 0 {
        return Ok(());
    }

    // This is where a player is: fetch around it, in order.
    session.seek(entry.offset + start);

    let mut pos = entry.offset + start;
    let stop = entry.offset + end + 1;
    let piece_len = session.torrent.piece_length.max(1);
    let mut woken = 0u32;
    while pos < stop {
        // Piece numbers are torrent-wide, so the file's own offset is added in.
        let abs_piece = (pos / piece_len) as usize;
        if !session.wait_for_piece(abs_piece, PIECE_WAIT).await {
            // Not ready yet and out of patience: stop cleanly. The client sees a
            // short body and retries the remainder — better than a hung socket.
            if woken == 0 {
                woken += 1;
                continue;
            }
            return Ok(());
        }
        // Re-arm the playhead as playback advances, so the readahead window moves
        // with the player instead of staying where it started.
        session.seek(pos);
        let next_piece_start = ((abs_piece + 1) as u64 * piece_len).min(stop);
        let run = (next_piece_start - pos).max(1).min(stop - pos);
        let mut buf = vec![0u8; run as usize];
        session.read_at(pos, &mut buf)?;
        if stream.write_all(&buf).await.is_err() {
            return Ok(()); // the player went away
        }
        pos += run;
    }
    Ok(())
}

type RangeSpec = Option<(Option<u64>, Option<u64>)>;

async fn read_request(stream: &mut TcpStream) -> Result<Option<(String, String, RangeSpec)>> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| OdmError::other(e.to_string()))?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 16384 {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.lines();
    let first = lines.next().unwrap_or("");
    let mut it = first.split_whitespace();
    let method = it.next().unwrap_or("").to_string();
    let path = it.next().unwrap_or("/").to_string();
    let mut range = None;
    for line in lines {
        if let Some(rest) = line.strip_prefix("Range:").or_else(|| {
            line.to_ascii_lowercase()
                .starts_with("range:")
                .then(|| &line[6..])
        }) {
            range = parse_range(rest.trim());
        }
        if line.is_empty() {
            break;
        }
    }
    Ok(Some((method, path, range)))
}

/// `bytes=start-end`, `bytes=start-`, `bytes=-suffix`.
pub fn parse_range(text: &str) -> RangeSpec {
    let spec = text.trim().strip_prefix("bytes=")?;
    // Multipart ranges are rare from players; serve the first one.
    let first = spec.split(',').next()?.trim();
    let (a, b) = first.split_once('-')?;
    let a = a.trim().parse::<u64>().ok();
    let b = b.trim().parse::<u64>().ok();
    match (a, b) {
        (None, None) => None,
        (a, b) => Some((a, b)),
    }
}

async fn write_head(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    headers: &[(&str, String)],
) -> Result<()> {
    let mut out = format!("HTTP/1.1 {status} {reason}\r\n");
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("Connection: close\r\n\r\n");
    stream
        .write_all(out.as_bytes())
        .await
        .map_err(|e| OdmError::other(e.to_string()))
}

async fn respond(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    headers: &[(&str, String)],
    body: &[u8],
) -> Result<()> {
    let mut all: Vec<(&str, String)> = headers.to_vec();
    all.push(("Content-Length", body.len().to_string()));
    write_head(stream, status, reason, &all).await?;
    let _ = stream.write_all(body).await;
    Ok(())
}

fn mime_for(name: &str) -> &'static str {
    match name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        "ts" => "video/mp2t",
        "mpg" | "mpeg" => "video/mpeg",
        "flv" => "video/x-flv",
        "mp3" => "audio/mpeg",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        "ogg" | "opus" => "audio/ogg",
        "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "srt" => "application/x-subrip",
        "vtt" => "text/vtt",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_are_read_the_three_ways_players_write_them() {
        assert_eq!(parse_range("bytes=0-1023"), Some((Some(0), Some(1023))));
        assert_eq!(parse_range("bytes=5000-"), Some((Some(5000), None)));
        assert_eq!(parse_range("bytes=-500"), Some((None, Some(500))));
        assert_eq!(parse_range("items=0-1"), None);
    }

    #[test]
    fn content_types_cover_what_torrents_contain() {
        assert_eq!(mime_for("movie.mp4"), "video/mp4");
        assert_eq!(mime_for("show.S01E01.mkv"), "video/x-matroska");
        assert_eq!(mime_for("README"), "application/octet-stream");
    }
}
