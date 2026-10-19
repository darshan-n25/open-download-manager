//! Minimal BitTorrent download engine: one torrent, one file, SHA-1-verified pieces.

use std::io::{Seek, SeekFrom, Write};
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::bt::torrent::Torrent;
use crate::error::{Result, RiptideError};

const BLOCK_SIZE: usize = 16 * 1024;
const PEER_ID: [u8; 20] = *b"-RT0001-0123456789ab";

pub async fn download(
    torrent: &Torrent,
    peers: &[crate::bt::tracker::Peer],
    out_path: &std::path::Path,
) -> Result<()> {
    // Preallocate output file
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(out_path)
        .map_err(|e| RiptideError::other(e.to_string()))?;
    file.set_len(torrent.length)
        .map_err(|e| RiptideError::other(e.to_string()))?;
    file.flush().ok();

    for pi in 0..torrent.num_pieces() {
        let psize = torrent.piece_size(pi) as usize;
        let mut buf = vec![0u8; psize];
        let mut ok = false;
        for peer in peers {
            let addr: SocketAddr = match format!("{}:{}", peer.ip, peer.port).parse() {
                Ok(a) => a,
                Err(_) => continue,
            };
            if let Ok(()) = download_piece(torrent, pi, &mut buf, addr).await {
                // verify
                use sha1::{Digest, Sha1};
                let mut h = Sha1::new();
                h.update(&buf);
                let got: [u8; 20] = h.finalize().into();
                if got == torrent.pieces[pi] {
                    // write to file
                    let offset = pi as u64 * torrent.piece_length;
                    // write under a mutex-ish: reopen append? use seek+write
                    let mut f = std::fs::OpenOptions::new()
                        .write(true)
                        .open(out_path)
                        .map_err(|e| RiptideError::other(e.to_string()))?;
                    f.seek(SeekFrom::Start(offset))
                        .map_err(|e| RiptideError::other(e.to_string()))?;
                    f.write_all(&buf)
                        .map_err(|e| RiptideError::other(e.to_string()))?;
                    f.flush().ok();
                    ok = true;
                    break;
                }
            }
        }
        if !ok {
            return Err(RiptideError::other(format!(
                "could not download piece {}",
                pi
            )));
        }
        eprintln!("piece {}/{} done", pi + 1, torrent.num_pieces());
    }
    Ok(())
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
    // handshake
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

    // read peer handshake (68 bytes)
    let mut peer_hs = [0u8; 68];
    stream
        .read_exact(&mut peer_hs)
        .await
        .map_err(|e| RiptideError::other(e.to_string()))?;
    if peer_hs[0] != 19 || &peer_hs[1..20] != b"BitTorrent protocol" {
        return Err(RiptideError::other("bad handshake"));
    }
    // verify info hash
    if peer_hs[28..48] != torrent.info_hash {
        return Err(RiptideError::other("info hash mismatch"));
    }

    // now read messages until we get unchoked, then request blocks
    // send interested
    send_msg(&mut stream, 2, &[]).await?;

    let psize = buf.len();
    let mut downloaded = 0usize;
    // block requests outstanding
    let mut next_begin = 0usize;
    let mut pending: std::collections::HashSet<usize> = std::collections::HashSet::new();

    // Re-open: we already consumed handshake via read_exact on stream. Continue using stream.
    let mut stream = stream;

    // request initial blocks
    while next_begin < psize && pending.len() < 5 {
        let len = BLOCK_SIZE.min(psize - next_begin);
        request_block(&mut stream, pi as u32, next_begin as u32, len as u32).await?;
        pending.insert(next_begin);
        next_begin += len;
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while downloaded < psize {
        if std::time::Instant::now() > deadline {
            return Err(RiptideError::other("piece timeout"));
        }
        // read a message
        let mut len_buf = [0u8; 4];
        // read with timeout
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
            0 => {} // choke - ignore
            1 => {
                // unchoke - request if not already
            }
            5 => {} // bitfield
            4 => {} // have
            7 => {
                // piece: piece_index(4) + begin(4) + block
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
                    // request next
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
