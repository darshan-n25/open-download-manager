//! Minimal BitTorrent download engine: one torrent, one file, SHA-1-verified pieces.

use std::io::{Seek, SeekFrom, Write};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::bt::torrent::Torrent;
use crate::error::{Result, RiptideError};

const BLOCK_SIZE: usize = 16 * 1024;
const PEER_ID: [u8; 20] = *b"-RT0001-0123456789ab";

/// How a torrent download is going.
#[derive(Clone, Copy, Debug, Default)]
pub struct TorrentProgress {
    /// Bytes committed to the output file.
    pub downloaded: u64,
    /// The total size, once the torrent has been read.
    pub total: u64,
    /// The current rate, in bytes per second.
    pub speed_bps: f64,
}

/// Reported as the download proceeds. The callback is called from the worker
/// thread, so it should be cheap and must not block.
pub type ProgressFn = Arc<dyn Fn(TorrentProgress) + Send + Sync>;

/// Download a torrent's single file, verifying every piece by its SHA-1.
///
/// `progress` is called as the transfer advances and `cancel` stops it between
/// pieces: a torrent has no partial file to resume from, so a cancelled
/// download starts again rather than continuing where it stopped.
pub async fn download(
    torrent: &Torrent,
    peers: &[crate::bt::tracker::Peer],
    out_path: &std::path::Path,
) -> Result<()> {
    download_with(torrent, peers, out_path, None, None).await
}

/// The same, with progress reporting and a way to stop.
pub async fn download_with(
    torrent: &Torrent,
    peers: &[crate::bt::tracker::Peer],
    out_path: &std::path::Path,
    progress: Option<ProgressFn>,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<()> {
    // Preallocate the output file, so a failed piece leaves a file of the right
    // size rather than a truncated one.
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(out_path)
        .map_err(|e| RiptideError::other(e.to_string()))?;
    file.set_len(torrent.length)
        .map_err(|e| RiptideError::other(e.to_string()))?;
    file.flush().ok();

    let total = torrent.length;
    let mut downloaded = 0u64;
    let started = std::time::Instant::now();

    for pi in 0..torrent.num_pieces() {
        if let Some(c) = &cancel {
            if c.load(Ordering::SeqCst) {
                return Err(RiptideError::other("torrent download cancelled"));
            }
        }
        let psize = torrent.piece_size(pi) as usize;
        let mut buf = vec![0u8; psize];
        let mut ok = false;
        for peer in peers {
            let addr: SocketAddr = match format!("{}:{}", peer.ip, peer.port).parse() {
                Ok(a) => a,
                Err(_) => continue,
            };
            if let Ok(()) = download_piece(torrent, pi, &mut buf, addr).await {
                // Verify the piece before it is written: a piece that does not hash
                // to what the torrent says is not the file, whatever the peer claims.
                use sha1::{Digest, Sha1};
                let mut h = Sha1::new();
                h.update(&buf);
                let got: [u8; 20] = h.finalize().into();
                if got == torrent.pieces[pi] {
                    // Write the piece at its own offset, so the pieces can arrive in
                    // any order.
                    let offset = pi as u64 * torrent.piece_length;
                    let mut f = std::fs::OpenOptions::new()
                        .write(true)
                        .open(out_path)
                        .map_err(|e| RiptideError::other(e.to_string()))?;
                    f.seek(SeekFrom::Start(offset))
                        .map_err(|e| RiptideError::other(e.to_string()))?;
                    f.write_all(&buf)
                        .map_err(|e| RiptideError::other(e.to_string()))?;
                    f.flush().ok();
                    downloaded += psize as u64;
                    ok = true;
                    break;
                }
            }
        }
        if !ok {
            return Err(RiptideError::other(format!(
                "could not download piece {pi} of {}",
                torrent.num_pieces()
            )));
        }
        if let Some(cb) = &progress {
            let elapsed = started.elapsed().as_secs_f64().max(0.001);
            cb(TorrentProgress {
                downloaded,
                total,
                speed_bps: downloaded as f64 / elapsed,
            });
        }
        eprintln!("piece {}/{} done", pi + 1, torrent.num_pieces());
    }
    Ok(())
}

/// Read a `.torrent` file.
pub fn read_torrent(path: &std::path::Path) -> Result<Torrent> {
    let data = std::fs::read(path).map_err(|e| RiptideError::other(e.to_string()))?;
    Torrent::from_bytes(&data)
}

/// Download whatever a magnet link points at.
///
/// A magnet carries a hash and some trackers but no file information, so the
/// torrent file itself is fetched from the swarm first; only then are the
/// pieces known well enough to be checked and written.
pub async fn download_magnet(
    magnet: &crate::bt::magnet::Magnet,
    out_dir: &std::path::Path,
    progress: Option<ProgressFn>,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<std::path::PathBuf> {
    if magnet.trackers.is_empty() {
        return Err(RiptideError::other(
            "the magnet link lists no tracker to ask for peers",
        ));
    }
    let peer_id = PEER_ID;

    // Ask each tracker in turn for peers, until one answers.
    let mut peers = Vec::new();
    let mut last_error = None;
    for tracker in &magnet.trackers {
        match crate::bt::tracker::announce_at(tracker, &magnet.info_hash, &peer_id, 6881, 0).await {
            Ok(p) if !p.is_empty() => {
                peers = p;
                break;
            }
            Ok(_) => {}
            Err(e) => last_error = Some(e),
        }
    }
    if peers.is_empty() {
        return Err(last_error.unwrap_or_else(|| {
            RiptideError::other("no tracker returned any peers for this magnet")
        }));
    }

    // The metadata comes from the peers themselves.
    let torrent = crate::bt::metadata::fetch(&magnet.info_hash, &peers).await?;

    let out_path = out_dir.join(&torrent.name);
    download_with(&torrent, &peers, &out_path, progress, cancel).await?;
    Ok(out_path)
}

async fn download_piece(
    torrent: &Torrent,
    pi: usize,
    buf: &mut [u8],
    addr: SocketAddr,
) -> Result<()> {
    let mut stream = TcpStream::connect(addr)
        .await
        .map_err(|e| RiptideError::other(e.to_string()))?;
    // The handshake: a protocol name, eight reserved bytes, the info hash and our
    // peer id. A peer that answers with a different info hash is talking about
    // a different torrent.
    let mut hs = Vec::with_capacity(68);
    hs.push(19);
    hs.extend_from_slice(b"BitTorrent protocol");
    hs.extend_from_slice(&[0u8; 8]);
    hs.extend_from_slice(&torrent.info_hash);
    hs.extend_from_slice(&PEER_ID);
    stream
        .write_all(&hs)
        .await
        .map_err(|e| RiptideError::other(e.to_string()))?;

    // Read the peer's handshake (68 bytes).
    let mut peer_hs = [0u8; 68];
    stream
        .read_exact(&mut peer_hs)
        .await
        .map_err(|e| RiptideError::other(e.to_string()))?;
    if peer_hs[0] != 19 || &peer_hs[1..20] != b"BitTorrent protocol" {
        return Err(RiptideError::other("bad handshake"));
    }
    // Verify the info hash.
    if peer_hs[28..48] != torrent.info_hash {
        return Err(RiptideError::other("info hash mismatch"));
    }

    // Ask for the piece.
    let psize = buf.len();
    let mut next_begin = 0usize;
    let mut pending: std::collections::HashSet<usize> = std::collections::HashSet::new();

    // Send `interested`, then request blocks.
    send_msg(&mut stream, 2, &[]).await?;

    while next_begin < psize && pending.len() < 5 {
        let len = BLOCK_SIZE.min(psize - next_begin);
        request_block(&mut stream, pi as u32, next_begin as u32, len as u32).await?;
        pending.insert(next_begin);
        next_begin += len;
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut downloaded = 0usize;
    while downloaded < psize {
        if std::time::Instant::now() > deadline {
            return Err(RiptideError::other("piece timeout"));
        }
        // Read a message.
        let mut len_buf = [0u8; 4];
        match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            stream.read_exact(&mut len_buf),
        )
        .await
        {
            Ok(Ok(_)) => {}
            _ => return Err(RiptideError::other("read timeout")),
        }
        let msg_len = u32::from_be_bytes(len_buf) as usize;
        if msg_len == 0 {
            continue; // keep-alive
        }
        let mut msg = vec![0u8; msg_len];
        stream
            .read_exact(&mut msg)
            .await
            .map_err(|e| RiptideError::other(e.to_string()))?;
        let msg_type = msg[0];
        match msg_type {
            0 => {} // choke
            1 => {} // unchoke
            5 => {} // bitfield
            4 => {} // have
            7 => {
                // piece: piece index (4), begin (4), then the block.
                if msg.len() < 9 {
                    continue;
                }
                let pidx = u32::from_be_bytes([msg[1], msg[2], msg[3], msg[4]]) as usize;
                if pidx != pi {
                    continue;
                }
                let begin = u32::from_be_bytes([msg[5], msg[6], msg[7], msg[8]]) as usize;
                let block = &msg[9..];
                if begin + block.len() <= buf.len() {
                    buf[begin..begin + block.len()].copy_from_slice(block);
                    downloaded += block.len();
                    pending.remove(&begin);
                    // Keep the pipe full.
                    while next_begin < psize && pending.len() < 5 {
                        let len = BLOCK_SIZE.min(psize - next_begin);
                        request_block(&mut stream, pi as u32, next_begin as u32, len as u32)
                            .await?;
                        pending.insert(next_begin);
                        next_begin += len;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

async fn send_msg(stream: &mut TcpStream, t: u8, payload: &[u8]) -> Result<()> {
    let mut buf = Vec::with_capacity(4 + 1 + payload.len());
    buf.extend_from_slice(&((1 + payload.len()) as u32).to_be_bytes());
    buf.push(t);
    buf.extend_from_slice(payload);
    stream
        .write_all(&buf)
        .await
        .map_err(|e| RiptideError::other(e.to_string()))
}

async fn request_block(stream: &mut TcpStream, piece: u32, begin: u32, length: u32) -> Result<()> {
    let mut payload = Vec::with_capacity(12);
    payload.extend_from_slice(&piece.to_be_bytes());
    payload.extend_from_slice(&begin.to_be_bytes());
    payload.extend_from_slice(&length.to_be_bytes());
    send_msg(stream, 6, &payload).await
}
