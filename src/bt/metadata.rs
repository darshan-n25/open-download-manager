//! Fetching a torrent's metadata from the swarm (BEP 9, "Metadata Exchange").
//!
//! A magnet link carries no piece hashes, so the only way to learn them is to
//! ask a peer: the `ut_metadata` extension lets a client request the torrent
//! file itself, in pieces, and the peers answer with it.

use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::bt::torrent::Torrent;
use crate::error::{Result, RiptideError};

const PEER_ID: [u8; 20] = *b"-RT0001-0123456789ab";
/// The extension message id we advertise for `ut_metadata`.
const OUR_METADATA_ID: u8 = 1;
/// The size of one metadata piece, as the extension defines it.
const METADATA_PIECE: usize = 16 * 1024;

/// Ask the peers for the torrent file and return it.
///
/// The peers come from the tracker, so the caller announces first.
pub async fn fetch(info_hash: &[u8; 20], peers: &[crate::bt::tracker::Peer]) -> Result<Torrent> {
    let mut last_error = None;
    for peer in peers {
        let addr: SocketAddr = match format!("{}:{}", peer.ip, peer.port).parse() {
            Ok(a) => a,
            Err(_) => continue,
        };
        match fetch_from_peer(info_hash, addr).await {
            Ok(t) => return Ok(t),
            Err(e) => last_error = Some(e),
        }
    }
    Err(last_error
        .unwrap_or_else(|| RiptideError::other("no peer could give us the torrent's metadata")))
}

/// The metadata, piece by piece, from one peer.
async fn fetch_from_peer(info_hash: &[u8; 20], addr: SocketAddr) -> Result<Torrent> {
    let mut stream = TcpStream::connect(addr)
        .await
        .map_err(|e| RiptideError::other(e.to_string()))?;

    // The handshake. The sixth reserved byte (index 5) says "I speak the
    // extension protocol"; without it the peer will not offer the metadata.
    // The byte's position is part of the protocol, not a detail: a peer that
    // looks at the wrong byte never answers, and the download simply hangs.
    let mut hs = Vec::with_capacity(68);
    hs.push(19);
    hs.extend_from_slice(b"BitTorrent protocol");
    hs.extend_from_slice(&[0, 0, 0, 0, 0, 0x10, 0, 0]);
    hs.extend_from_slice(info_hash);
    hs.extend_from_slice(&PEER_ID);
    stream
        .write_all(&hs)
        .await
        .map_err(|e| RiptideError::other(e.to_string()))?;

    let mut peer_hs = [0u8; 68];
    stream
        .read_exact(&mut peer_hs)
        .await
        .map_err(|e| RiptideError::other(e.to_string()))?;
    if &peer_hs[28..48] != info_hash {
        return Err(RiptideError::other(
            "the peer is talking about another torrent",
        ));
    }

    // The peer's extension handshake: which extensions it supports. It does not
    // arrive first — the bitfield usually does — so keep reading until the
    // handshake turns up.
    let mut peer_metadata_id: Option<u8> = None;
    let mut buf = Vec::new();
    for _ in 0..8 {
        let msg = read_message(&mut stream, &mut buf).await?;
        if msg.len() < 2 {
            continue;
        }
        let msg_id = u16::from_be_bytes([msg[0], msg[1]]);
        let payload = &msg[2..];
        if msg_id == 0 && payload.starts_with(b"m:ut_metadata") && payload.len() >= 14 {
            // "m:ut_metadata" is thirteen bytes, so the peer's id for the
            // extension is the fourteenth.
            peer_metadata_id = Some(payload[13]);
            break;
        }
    }
    let Some(peer_metadata_id) = peer_metadata_id else {
        return Err(RiptideError::other(
            "the peer does not offer the metadata extension",
        ));
    };

    // Ours: "m:ut_metadata", our id, and a bitfield of the metadata pieces we
    // have — none.
    let mut ours = Vec::new();
    ours.extend_from_slice(b"m:ut_metadata");
    ours.push(OUR_METADATA_ID);
    ours.extend_from_slice(&[0, 0, 0, 0]);
    write_extension(&mut stream, 0, &ours).await?;

    // Ask for the first piece of the metadata. A data message's payload starts
    // with the extension's own id, then the request.
    let mut query = Vec::new();
    query.push(peer_metadata_id);
    query.push(0u8); // "query"
    query.extend_from_slice(&0u32.to_be_bytes());
    write_extension(&mut stream, 1, &query).await?;

    // Collect the pieces until we have the whole thing.
    let mut data: Vec<u8> = Vec::new();
    let mut total: Option<u32> = None;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    while total.is_none() || data.len() < total.unwrap() as usize {
        if tokio::time::Instant::now() > deadline {
            return Err(RiptideError::other("timed out waiting for the metadata"));
        }
        let msg = read_message(&mut stream, &mut buf).await?;
        if msg.len() < 3 {
            continue;
        }
        let msg_id = u16::from_be_bytes([msg[0], msg[1]]);
        if msg_id != 1 {
            continue;
        }
        let payload = &msg[2..];
        if payload.len() < 10 {
            continue;
        }
        if payload[0] != peer_metadata_id {
            continue;
        }
        let kind = payload[1];
        if kind != 1 {
            // 0 is a query, 2 is a rejection.
            continue;
        }
        let piece = u32::from_be_bytes([payload[2], payload[3], payload[4], payload[5]]);
        let size = u32::from_be_bytes([payload[6], payload[7], payload[8], payload[9]]);
        let chunk = &payload[10..];
        if total.is_none() {
            total = Some(size);
            data = vec![0u8; size as usize];
        }
        let start = piece as usize * METADATA_PIECE;
        if start + chunk.len() > data.len() {
            continue;
        }
        data[start..start + chunk.len()].copy_from_slice(chunk);
    }

    Torrent::from_metadata(&data, *info_hash)
}

/// Read one length-prefixed message.
async fn read_message(stream: &mut TcpStream, scratch: &mut Vec<u8>) -> Result<Vec<u8>> {
    scratch.clear();
    scratch.resize(4, 0);
    stream
        .read_exact(scratch)
        .await
        .map_err(|e| RiptideError::other(e.to_string()))?;
    let len = u32::from_be_bytes([scratch[0], scratch[1], scratch[2], scratch[3]]) as usize;
    if len > 1 << 20 {
        return Err(RiptideError::other("an oversized extension message"));
    }
    scratch.clear();
    scratch.resize(len, 0);
    stream
        .read_exact(scratch)
        .await
        .map_err(|e| RiptideError::other(e.to_string()))?;
    Ok(scratch.clone())
}

/// Write one length-prefixed extension message.
///
/// The message id is two bytes: 0 for the extension handshake, 1 for the data
/// channel that follows it.
async fn write_extension(stream: &mut TcpStream, msg_id: u16, payload: &[u8]) -> Result<()> {
    let mut body = Vec::with_capacity(2 + payload.len());
    body.extend_from_slice(&msg_id.to_be_bytes());
    body.extend_from_slice(payload);
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    stream
        .write_all(&out)
        .await
        .map_err(|e| RiptideError::other(e.to_string()))
}
