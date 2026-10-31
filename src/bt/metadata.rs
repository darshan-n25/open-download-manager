//! Fetching a torrent's metadata from the swarm (BEP 9, "Metadata Exchange").
//!
//! A magnet link carries no piece hashes, so the only way to learn them is to
//! ask a peer: the `ut_metadata` extension lets a client request the torrent
//! file itself, in pieces, and the peers answer with it.
//!
//! The wire format is BEP 10: an extension message always starts with message
//! id 20, then the *extension's* one-byte id, then the extension payload. The
//! extension handshake is id 0; a `ut_metadata` request/data message is the id
//! the other side advertised in its handshake. Both ids are one byte. Reading
//! them as a two-byte integer produces a private dialect that no normal client
//! speaks, so this module is deliberately strict about the layout.

use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::bt::bencode;
use crate::bt::torrent::Torrent;
use crate::error::{OdmError, Result};

const PEER_ID: [u8; 20] = *b"-RT0001-0123456789ab";
/// BEP 10's message id for every extension message.
const EXTENDED: u8 = 20;
/// The extension id we advertise for `ut_metadata`.
const OUR_METADATA_ID: u8 = 1;
/// The size of one metadata piece, as the extension defines it.
const METADATA_PIECE: usize = 16 * 1024;
/// Our extension handshake: advertise `ut_metadata` as id 1. `metadata_size`
/// is deliberately absent: BEP 9 says to omit it when we do not have the
/// metadata yet, and some peers treat a zero-sized claim as an invalid one.
const OUR_HANDSHAKE: &[u8] = b"d1:md11:ut_metadatai1eee";
/// A read that takes longer than this is treated as a dead peer.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);
/// The whole metadata exchange, across all pieces.
const EXCHANGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
/// How many peers are asked at the same time.
const FETCH_PARALLEL: usize = 24;
/// How long one peer is given to accept the connection.
const FETCH_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6);
/// The whole search, so a magnet with a dead swarm fails in a bounded time instead
/// of spinning forever behind the interface.
const FETCH_BUDGET: std::time::Duration = std::time::Duration::from_secs(120);

/// Ask the peers for the torrent file and return it.
///
/// Many peers at once: a public swarm's tracker list is mostly addresses that never
/// answer, so asking one at a time would spend minutes on dead peers before reaching
/// a live one. The first peer that delivers the metadata wins, and the rest are
/// dropped.
pub async fn fetch(info_hash: &[u8; 20], peers: &[crate::bt::tracker::Peer]) -> Result<Torrent> {
    let addrs: Vec<SocketAddr> = peers.iter().map(|p| p.addr).collect();
    if addrs.is_empty() {
        return Err(OdmError::other("no peers to ask for the metadata"));
    }
    let deadline = tokio::time::Instant::now() + FETCH_BUDGET;
    let mut next = 0usize;
    let mut set = tokio::task::JoinSet::new();
    while set.len() < FETCH_PARALLEL && next < addrs.len() {
        set.spawn(fetch_from_peer_owned(*info_hash, addrs[next]));
        next += 1;
    }

    let mut last_error = None;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() || set.is_empty() {
            break;
        }
        match tokio::time::timeout(left, set.join_next()).await {
            Ok(Some(Ok((addr, Ok(torrent))))) => {
                tracing::debug!(%addr, "metadata received");
                set.abort_all();
                return Ok(torrent);
            }
            Ok(Some(Ok((addr, Err(e))))) => {
                tracing::debug!(%addr, "metadata: {e}");
                last_error = Some(e);
            }
            Ok(Some(Err(e))) => last_error = Some(OdmError::other(format!("metadata: {e}"))),
            Ok(None) => break,
            Err(_) => break,
        }
        if next < addrs.len() {
            set.spawn(fetch_from_peer_owned(*info_hash, addrs[next]));
            next += 1;
        }
    }
    set.abort_all();
    Err(last_error
        .unwrap_or_else(|| OdmError::other("no peer could give us the torrent's metadata")))
}

async fn fetch_from_peer_owned(
    info_hash: [u8; 20],
    addr: SocketAddr,
) -> (SocketAddr, Result<Torrent>) {
    (addr, fetch_from_peer(&info_hash, addr).await)
}

/// The metadata, piece by piece, from one peer.
async fn fetch_from_peer(info_hash: &[u8; 20], addr: SocketAddr) -> Result<Torrent> {
    // A peer that does not answer the SYN promptly is not going to hand over the
    // metadata either; holding a slot on it for the OS's connect timeout wastes the
    // whole budget.
    let mut stream = tokio::time::timeout(FETCH_CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| OdmError::other("timed out connecting to the peer"))?
        .map_err(|e| OdmError::other(e.to_string()))?;

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
        .map_err(|e| OdmError::other(e.to_string()))?;

    let mut peer_hs = [0u8; 68];
    read_exact(&mut stream, &mut peer_hs).await?;
    if &peer_hs[28..48] != info_hash {
        return Err(OdmError::other("the peer is talking about another torrent"));
    }

    // The peer's extension handshake: which extensions it supports. It does not
    // arrive first — the bitfield usually does — so keep reading until the
    // handshake turns up.
    let mut peer_metadata_id: Option<u8> = None;
    for _ in 0..12 {
        let msg = read_message(&mut stream).await?;
        if msg.len() < 2 || msg[0] != EXTENDED || msg[1] != 0 {
            continue;
        }
        let Ok(handshake) = bencode::decode_one_doc(&msg[2..]) else {
            continue;
        };
        let Some(m) = handshake.dict_get(b"m") else {
            continue;
        };
        let Some(id) = m.dict_get_int(b"ut_metadata") else {
            continue;
        };
        let id = u8::try_from(id)
            .map_err(|_| OdmError::other("the peer advertised an invalid metadata id"))?;
        if id == 0 || id == EXTENDED {
            return Err(OdmError::other(
                "the peer advertised an invalid metadata id",
            ));
        }
        peer_metadata_id = Some(id);
        break;
    }
    let Some(peer_metadata_id) = peer_metadata_id else {
        return Err(OdmError::other(
            "the peer does not offer the metadata extension",
        ));
    };

    // Advertise our side, then ask for the first piece.
    write_extension(&mut stream, 0, OUR_HANDSHAKE).await?;
    write_extension(&mut stream, peer_metadata_id, &request_piece(0)).await?;

    // Collect the pieces until we have the whole thing.
    let mut data: Vec<u8> = Vec::new();
    let mut total: Option<usize> = None;
    // Which pieces have actually arrived. The buffer is allocated at full size as
    // soon as the first reply names the size, so its length says nothing about
    // completeness — an earlier version stopped right there and parsed zeros.
    let mut have: Vec<bool> = Vec::new();
    // Piece 0 was requested above; the rest are requested as soon as the first
    // reply tells us how large the metadata is.
    let mut requested = 1usize;
    let deadline = tokio::time::Instant::now() + EXCHANGE_TIMEOUT;
    loop {
        if total.is_some() && have.iter().all(|bit| *bit) {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            return Err(OdmError::other("timed out waiting for the metadata"));
        }
        let msg = read_message(&mut stream).await?;
        if msg.len() < 2 || msg[0] != EXTENDED {
            continue;
        }
        // Data comes back on *our* advertised id, not the peer's.
        if msg[1] != OUR_METADATA_ID {
            continue;
        }
        let payload = &msg[2..];
        if payload.is_empty() {
            continue;
        }
        // ut_metadata messages are bencoded dictionaries, and a data message
        // appends the raw piece after the dictionary. The old parser read a
        // fixed binary header, which no standard peer sends.
        let Ok((dict, consumed)) = bencode::decode(payload) else {
            continue;
        };
        let Some(kind) = dict.dict_get_int(b"msg_type") else {
            continue;
        };
        match kind {
            // 0 is a query, 1 is data, 2 is a rejection.
            1 => {
                let Some(piece) = dict.dict_get_int(b"piece") else {
                    continue;
                };
                let Some(size) = dict.dict_get_int(b"total_size") else {
                    continue;
                };
                let (Ok(piece), Ok(size)) = (usize::try_from(piece), usize::try_from(size)) else {
                    continue;
                };
                let chunk = &payload[consumed..];
                match total {
                    None => {
                        // The size is a peer's claim, and it decides how much memory
                        // is allocated. Torrent metadata is a few kilobytes to a few
                        // megabytes; anything wildly outside that is not worth
                        // believing.
                        if size == 0 || size > 32 * 1024 * 1024 {
                            return Err(OdmError::other(format!(
                                "the peer claims {size} bytes of metadata"
                            )));
                        }
                        total = Some(size);
                        data = vec![0u8; size];
                        have = vec![false; size.div_ceil(METADATA_PIECE)];
                    }
                    // Every peer must agree on the size, or the pieces will not fit.
                    Some(n) if n != size => continue,
                    Some(_) => {}
                }
                // A peer can answer with a piece we did not ask for, or with one
                // that does not fit the size it just gave us. Either way it is not
                // metadata we can use; ignoring it is safer than panicking on it.
                let start = piece * METADATA_PIECE;
                if piece < have.len() && start + chunk.len() <= data.len() {
                    data[start..start + chunk.len()].copy_from_slice(chunk);
                    have[piece] = true;
                }
                // Ask for every remaining piece once. A small torrent has one;
                // a large one has several, and the old code only ever asked
                // for piece zero and then waited forever.
                let pieces = have.len();
                while requested < pieces {
                    write_extension(
                        &mut stream,
                        peer_metadata_id,
                        &request_piece(requested as u32),
                    )
                    .await?;
                    requested += 1;
                }
            }
            2 => return Err(OdmError::other("the peer rejected the metadata request")),
            _ => {}
        }
    }

    Torrent::from_metadata(&data, *info_hash)
}

/// The `ut_metadata` request: a bencoded dict, `msg_type` 0, then the piece.
fn request_piece(piece: u32) -> Vec<u8> {
    format!("d8:msg_typei0e5:piecei{piece}ee").into_bytes()
}

/// Read exactly `buf.len()` bytes, treating a silent peer as dead.
async fn read_exact(stream: &mut TcpStream, buf: &mut [u8]) -> Result<()> {
    match tokio::time::timeout(READ_TIMEOUT, stream.read_exact(buf)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(OdmError::other(e.to_string())),
        Err(_) => Err(OdmError::other("timed out reading from the peer")),
    }
}

/// Read one length-prefixed message.
async fn read_message(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut len = [0u8; 4];
    read_exact(stream, &mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > 1 << 20 {
        return Err(OdmError::other("an oversized extension message"));
    }
    let mut body = vec![0u8; len];
    read_exact(stream, &mut body).await?;
    Ok(body)
}

/// Write one length-prefixed extension message: 20, the extension id, payload.
async fn write_extension(stream: &mut TcpStream, ext_id: u8, payload: &[u8]) -> Result<()> {
    let mut body = Vec::with_capacity(2 + payload.len());
    body.push(EXTENDED);
    body.push(ext_id);
    body.extend_from_slice(payload);
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    stream
        .write_all(&out)
        .await
        .map_err(|e| OdmError::other(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bt::bencode::Value;

    #[test]
    fn request_piece_is_a_bencoded_dict() {
        let v = bencode::decode_one_doc(&request_piece(0)).unwrap();
        assert_eq!(v.dict_get_int(b"msg_type"), Some(0));
        assert_eq!(v.dict_get_int(b"piece"), Some(0));

        let v = bencode::decode_one_doc(&request_piece(257)).unwrap();
        assert_eq!(v.dict_get_int(b"piece"), Some(257));
    }

    #[test]
    fn our_handshake_parses_and_advertises_metadata() {
        let v = bencode::decode_one_doc(OUR_HANDSHAKE).unwrap();
        assert_eq!(
            v.dict_get(b"m")
                .and_then(|m| m.dict_get_int(b"ut_metadata")),
            Some(1)
        );
        assert_eq!(v.dict_get(b"metadata_size").and_then(Value::as_int), None);
    }
}
