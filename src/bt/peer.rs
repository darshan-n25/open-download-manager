//! One peer connection, kept alive until the swarm is done with it.
//!
//! The expensive thing about this client used to be the connection: a fresh TCP
//! handshake for every piece. A connection here costs one handshake and then carries
//! whatever pieces the picker hands out, with a request pipeline deep enough that
//! bandwidth is limited by the peer, not by the round-trip time.
//!
//! It speaks the parts of the protocol that matter for downloading: the base
//! handshake, `have`/`bitfield`, `request`/`piece`, the extension protocol (BEP 10)
//! for `ut_pex` (BEP 11) and `ut_metadata` (BEP 9), and the DHT port message.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::bt::bencode::{self, Value};
use crate::bt::picker::{Request, BLOCK};
use crate::bt::session::Session;
use crate::error::{OdmError, Result};

const PROTOCOL: &[u8] = b"BitTorrent protocol";
const EXTENDED: u8 = 20;
/// The id we advertise for `ut_metadata`.
const OUR_METADATA_ID: u8 = 1;
/// The id we advertise for `ut_pex`.
const OUR_PEX_ID: u8 = 2;

/// A peer that has not accepted the connection by now is not worth the slot: in a
/// public swarm most addresses are stale, and the manager must get through the list.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(25);
const SNUB_AFTER: Duration = Duration::from_secs(45);
/// Bigger than any legal message (a block is 16 KiB); a peer claiming one is
/// either broken or trying to make us allocate its problem.
const MAX_MESSAGE: usize = 2 * 1024 * 1024;

/// Download one connection's worth of pieces, then return when it is no longer useful.
pub async fn run(session: Arc<Session>, addr: SocketAddr) -> Result<()> {
    if session.is_done() || session.is_cancelled() {
        return Ok(());
    }
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| OdmError::other("connect timed out"))?
        .map_err(|e| OdmError::other(format!("connect: {e}")))?;
    stream.set_nodelay(true).ok();
    let (mut rd, mut wr) = stream.into_split();

    handshake(&session, addr, &mut rd, &mut wr).await?;

    let _guard = PeerGuard {
        session: session.clone(),
        addr,
    };
    session.register_peer(addr);

    // Ask for the whole bitfield, then tell the peer we want something.
    write_msg(&mut wr, 2, &[]).await?;

    // Reads happen in their own task, not inside the `select!` below. A `select!`
    // cancels the future that loses the race, and a cancelled `read_exact` keeps
    // whatever bytes it had already taken off the socket — which leaves the stream
    // mid-frame and every later message garbage. A task that only ever reads cannot
    // be cancelled half-way through a message.
    let (frame_tx, mut frame_rx) = mpsc::channel::<Vec<u8>>(32);
    tokio::spawn(async move {
        let mut rd = rd;
        loop {
            match read_frame(&mut rd).await {
                Ok(frame) => {
                    if frame_tx.send(frame).await.is_err() {
                        return; // the connection loop has gone away
                    }
                }
                Err(_) => return, // closed, timed out, or not speaking the protocol
            }
        }
    });

    let mut pipeline: HashMap<(usize, u32), Instant> = HashMap::new();
    let mut choked = true;
    let mut peer_pex_id: Option<u8> = None;
    let mut last_pex = Instant::now();
    let mut target = 8usize;
    let mut bytes_at = Instant::now();
    let mut bytes_then = 0u64;

    let mut ticker = tokio::time::interval(Duration::from_millis(500));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        if session.is_done() || session.is_cancelled() || session.is_banned(addr) {
            return Ok(());
        }
        tokio::select! {
            biased;
            _ = ticker.tick() => {
                // Give up on blocks this peer has sat on.
                let now = Instant::now();
                let stale: Vec<Request> = pipeline
                    .iter()
                    .filter(|(_, at)| now.duration_since(**at) > REQUEST_TIMEOUT)
                    .map(|((piece, begin), _)| Request { piece: *piece, begin: *begin, len: 0 })
                    .collect();
                for req in &stale {
                    pipeline.remove(&(req.piece, req.begin));
                    session.release(Request { piece: req.piece, begin: req.begin, len: 0 });
                }
                if !choked && pipeline.is_empty() && !stale.is_empty() {
                    // Nothing came back at all for a whole round of requests.
                    return Err(OdmError::other("peer stopped responding"));
                }
                // Two seconds of in-flight data at the rate this peer has shown.
                let dt = bytes_at.elapsed().as_secs_f64();
                if dt >= 2.0 {
                    let rate = (session.peer_progress(addr).saturating_sub(bytes_then)) as f64 / dt;
                    target = ((rate / BLOCK as f64) * 2.0).clamp(8.0, 64.0) as usize;
                    bytes_at = Instant::now();
                    bytes_then = session.peer_progress(addr);
                }
                if !choked {
                    fill(&session, addr, &mut wr, &mut pipeline, target).await?;
                }
                if choked && !pipeline.is_empty() && session.peer_is_stale(addr, SNUB_AFTER) {
                    return Err(OdmError::other("peer is snubbed"));
                }
                if let Some(id) = peer_pex_id {
                    if last_pex.elapsed() > Duration::from_secs(60) {
                        let payload = pex_message(&session, addr);
                        write_ext(&mut wr, id, &payload).await?;
                        last_pex = Instant::now();
                    }
                }
            }
            maybe = frame_rx.recv() => {
                // `None` means the reader task stopped: the peer hung up, went
                // quiet, or broke the framing. Either way this connection is done
                // and the peer manager will decide whether to try again.
                let Some(frame) = maybe else {
                    return Ok(());
                };
                if frame.is_empty() {
                    continue; // keep-alive
                }
                let id = frame[0];
                let buf = &frame[..];
                match id {
                    255 => {} // keep-alive
                    0 => {
                        choked = true;
                        for (piece, begin) in pipeline.keys() {
                            session.release(Request { piece: *piece, begin: *begin, len: 0 });
                        }
                        pipeline.clear();
                    }
                    1 => {
                        choked = false;
                        fill(&session, addr, &mut wr, &mut pipeline, target).await?;
                    }
                    4 => {
                        if buf.len() >= 5 {
                            let piece = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
                            if piece < session.piece_count() {
                                session.set_peer_bit(addr, piece);
                            }
                        }
                    }
                    5 => {
                        let bits = &buf[1..];
                        session.set_peer_have(addr, bits.to_vec());
                    }
                    7 => {
                        if buf.len() < 9 {
                            continue;
                        }
                        let piece = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
                        let begin = u32::from_be_bytes([buf[5], buf[6], buf[7], buf[8]]);
                        // Only a block we asked for is worth keeping; unsolicited
                        // data is how a malicious peer would try to poison a piece.
                        if pipeline.remove(&(piece, begin)).is_none() {
                            continue;
                        }
                        let len = (buf.len() - 9) as u32;
                        session.on_block(addr, Request { piece, begin, len }, &buf[9..]).await;
                        if session.is_done() {
                            return Ok(());
                        }
                        if !choked {
                            fill(&session, addr, &mut wr, &mut pipeline, target).await?;
                        }
                    }
                    20 => {
                        if buf.len() < 2 {
                            continue;
                        }
                        let ext = buf[1];
                        if ext == 0 {
                            if let Ok(v) = bencode::decode_one_doc(&buf[2..]) {
                                peer_pex_id = v
                                    .dict_get(b"m")
                                    .and_then(|m| m.dict_get_int(b"ut_pex"))
                                    .and_then(|n| u8::try_from(n).ok())
                                    .filter(|n| *n != 0);
                            }
                        } else if ext == OUR_PEX_ID {
                            if let Ok(v) = bencode::decode_one_doc(&buf[2..]) {
                                for addr in parse_pex(&v) {
                                    session.add_peer(addr);
                                }
                            }
                        } else if ext == OUR_METADATA_ID {
                            // Another leecher is asking us for the info dictionary.
                            // We have it, so serve it — it costs nothing and makes
                            // the swarm healthier.
                            if let Ok((v, consumed)) = bencode::decode(&buf[2..]) {
                                if v.dict_get_int(b"msg_type") == Some(0) {
                                    if let Some(piece) = v.dict_get_int(b"piece") {
                                        let reply = metadata_reply(&session, piece as usize);
                                        if !reply.is_empty() {
                                            write_ext(&mut wr, OUR_METADATA_ID, &reply).await?;
                                        }
                                    }
                                    let _ = consumed;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

struct PeerGuard {
    session: Arc<Session>,
    addr: SocketAddr,
}

impl Drop for PeerGuard {
    fn drop(&mut self) {
        self.session.drop_peer(self.addr);
    }
}

async fn handshake(
    session: &Session,
    addr: SocketAddr,
    rd: &mut OwnedReadHalf,
    wr: &mut OwnedWriteHalf,
) -> Result<()> {
    let mut hs = Vec::with_capacity(68);
    hs.push(PROTOCOL.len() as u8);
    hs.extend_from_slice(PROTOCOL);
    // Extension protocol (byte 5, 0x10) and DHT (byte 7, 0x01). No fast extension:
    // it changes how `have` is encoded, and nothing here needs it.
    hs.extend_from_slice(&[0, 0, 0, 0, 0, 0x10, 0, 0x01]);
    hs.extend_from_slice(&session.info_hash());
    hs.extend_from_slice(&session.peer_id());
    wr.write_all(&hs)
        .await
        .map_err(|e| OdmError::other(format!("handshake write: {e}")))?;

    let mut peer_hs = [0u8; 68];
    tokio::time::timeout(HANDSHAKE_TIMEOUT, rd.read_exact(&mut peer_hs))
        .await
        .map_err(|_| OdmError::other("handshake timed out"))?
        .map_err(|e| OdmError::other(format!("handshake read: {e}")))?;
    if peer_hs[0] as usize != PROTOCOL.len() || &peer_hs[1..20] != PROTOCOL {
        return Err(OdmError::other("peer does not speak BitTorrent"));
    }
    if peer_hs[28..48] != session.info_hash() {
        return Err(OdmError::other(format!(
            "{addr} is serving another torrent"
        )));
    }

    if peer_hs[25] & 0x10 != 0 {
        let mut handshake = Vec::new();
        handshake.extend_from_slice(b"d1:md11:ut_metadatai1e6:ut_pexi2ee1:p");
        handshake.extend_from_slice(format!("i{}e", session.port()).as_bytes());
        handshake.extend_from_slice(b"1:v6:ODMbt1e");
        write_ext(wr, 0, &handshake).await?;
    }
    Ok(())
}

/// Keep the pipe full: request until `target` blocks are outstanding.
async fn fill(
    session: &Session,
    addr: SocketAddr,
    wr: &mut OwnedWriteHalf,
    pipeline: &mut HashMap<(usize, u32), Instant>,
    target: usize,
) -> Result<()> {
    while pipeline.len() < target {
        let Some(req) = session.pick(addr, Instant::now()) else {
            break;
        };
        // During the endgame the picker answers with blocks that are already
        // requested elsewhere; if this connection already has that block outstanding,
        // writing it again would not make the pipeline longer — and looping here
        // would keep us from reading the replies already on their way.
        if pipeline.contains_key(&(req.piece, req.begin)) {
            break;
        }
        pipeline.insert((req.piece, req.begin), Instant::now());
        let mut payload = [0u8; 12];
        payload[0..4].copy_from_slice(&(req.piece as u32).to_be_bytes());
        payload[4..8].copy_from_slice(&req.begin.to_be_bytes());
        payload[8..12].copy_from_slice(&req.len.to_be_bytes());
        write_msg(wr, 6, &payload).await?;
    }
    Ok(())
}

async fn read_frame(rd: &mut OwnedReadHalf) -> Result<Vec<u8>> {
    let mut len = [0u8; 4];
    tokio::time::timeout(IDLE_TIMEOUT, rd.read_exact(&mut len))
        .await
        .map_err(|_| OdmError::other("peer went quiet"))?
        .map_err(|e| OdmError::other(format!("read: {e}")))?;
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 {
        // Keep-alive: no id, no payload.
        return Ok(Vec::new());
    }
    if len > MAX_MESSAGE {
        return Err(OdmError::other("peer sent an oversized message"));
    }
    let mut frame = vec![0u8; len];
    tokio::time::timeout(IDLE_TIMEOUT, rd.read_exact(&mut frame))
        .await
        .map_err(|_| OdmError::other("peer went quiet mid-message"))?
        .map_err(|e| OdmError::other(format!("read: {e}")))?;
    Ok(frame)
}

async fn write_msg(wr: &mut OwnedWriteHalf, id: u8, payload: &[u8]) -> Result<()> {
    let mut head = [0u8; 5];
    head[0..4].copy_from_slice(&((payload.len() + 1) as u32).to_be_bytes());
    head[4] = id;
    wr.write_all(&head)
        .await
        .map_err(|e| OdmError::other(format!("write: {e}")))?;
    if !payload.is_empty() {
        wr.write_all(payload)
            .await
            .map_err(|e| OdmError::other(format!("write: {e}")))?;
    }
    Ok(())
}

async fn write_ext(wr: &mut OwnedWriteHalf, ext: u8, payload: &[u8]) -> Result<()> {
    let mut body = Vec::with_capacity(payload.len() + 1);
    body.push(ext);
    body.extend_from_slice(payload);
    write_msg(wr, EXTENDED, &body).await
}

/// Peer exchange: tell a peer who else we are connected to.
fn pex_message(session: &Session, except: SocketAddr) -> Vec<u8> {
    let addrs: Vec<SocketAddr> = session
        .peer_addrs()
        .into_iter()
        .filter(|a| *a != except && a.is_ipv4())
        .take(50)
        .collect();
    let mut added = Vec::with_capacity(addrs.len() * 6);
    for a in &addrs {
        if let SocketAddr::V4(v4) = a {
            added.extend_from_slice(&v4.ip().octets());
            added.extend_from_slice(&v4.port().to_be_bytes());
        }
    }
    let flags = vec![0u8; addrs.len()];
    let mut out = Vec::new();
    out.extend_from_slice(b"d5:added");
    out.extend_from_slice(format!("{}:", added.len()).as_bytes());
    out.extend_from_slice(&added);
    out.extend_from_slice(b"7:added.f");
    out.extend_from_slice(format!("{}:", flags.len()).as_bytes());
    out.extend_from_slice(&flags);
    out.extend_from_slice(b"7:dropped0:e");
    out
}

fn parse_pex(v: &Value) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    if let Some(added) = v.dict_get(b"added").and_then(Value::as_bytes) {
        for chunk in added.as_chunks::<6>().0 {
            let ip = std::net::Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
            let port = u16::from_be_bytes([chunk[4], chunk[5]]);
            if port != 0 && !ip.is_unspecified() && !ip.is_loopback() {
                out.push(SocketAddr::from((ip, port)));
            }
        }
    }
    out
}

fn metadata_reply(session: &Session, piece: usize) -> Vec<u8> {
    const SIZE: usize = 16 * 1024;
    let bytes = &session.torrent.info_bytes;
    let start = piece * SIZE;
    if start >= bytes.len() {
        return format!("d8:msg_typei2e5:piecei{piece}ee").into_bytes();
    }
    let end = (start + SIZE).min(bytes.len());
    let mut out = Vec::new();
    out.extend_from_slice(
        format!(
            "d8:msg_typei1e5:piecei{piece}e10:total_sizei{}ee",
            bytes.len()
        )
        .as_bytes(),
    );
    out.extend_from_slice(&bytes[start..end]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pex_added_is_six_bytes_per_peer() {
        let mut body = Vec::new();
        let peers = vec![
            SocketAddr::from(([10, 0, 0, 1], 6881)),
            SocketAddr::from(([10, 0, 0, 2], 51413)),
        ];
        let mut added = Vec::new();
        for a in &peers {
            if let SocketAddr::V4(v4) = a {
                added.extend_from_slice(&v4.ip().octets());
                added.extend_from_slice(&v4.port().to_be_bytes());
            }
        }
        body.extend_from_slice(b"d5:added");
        body.extend_from_slice(format!("{}:", added.len()).as_bytes());
        body.extend_from_slice(&added);
        body.extend_from_slice(b"e");
        let v = bencode::decode_one_doc(&body).unwrap();
        assert_eq!(parse_pex(&v), peers);
    }

    #[test]
    fn pex_drops_loopback_and_zero_ports() {
        let mut added = Vec::new();
        added.extend_from_slice(&[127, 0, 0, 1]);
        added.extend_from_slice(&6881u16.to_be_bytes());
        added.extend_from_slice(&[10, 1, 1, 1]);
        added.extend_from_slice(&0u16.to_be_bytes());
        added.extend_from_slice(&[10, 1, 1, 2]);
        added.extend_from_slice(&6881u16.to_be_bytes());
        let mut body = Vec::new();
        body.extend_from_slice(b"d5:added18:");
        body.extend_from_slice(&added);
        body.extend_from_slice(b"e");
        let v = bencode::decode_one_doc(&body).unwrap();
        assert_eq!(parse_pex(&v), vec![SocketAddr::from(([10, 1, 1, 2], 6881))]);
    }
}
