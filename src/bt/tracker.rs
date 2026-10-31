//! Tracker announces (BEP 3 HTTP, BEP 15 UDP) and the peer addresses they return.
//!
//! Most magnet links in the wild carry only `udp://` trackers, so an HTTP-only
//! announcer fails on exactly the links people paste. Both transports produce the
//! same [`Announce`] answer here, and the caller never has to care which one ran.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use crate::bt::bencode;
use crate::error::{OdmError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peer {
    pub addr: SocketAddr,
}

impl Peer {
    pub fn new(addr: SocketAddr) -> Peer {
        Peer { addr }
    }
}

impl From<SocketAddr> for Peer {
    fn from(addr: SocketAddr) -> Peer {
        Peer { addr }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Started,
    Completed,
    Stopped,
    None,
}

impl Event {
    fn as_str(self) -> &'static str {
        match self {
            Event::Started => "started",
            Event::Completed => "completed",
            Event::Stopped => "stopped",
            Event::None => "",
        }
    }

    fn as_u32(self) -> u32 {
        match self {
            Event::None => 0,
            Event::Completed => 1,
            Event::Started => 2,
            Event::Stopped => 3,
        }
    }
}

/// What the tracker is told about this session.
#[derive(Debug, Clone)]
pub struct Announce {
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
    /// The port we listen on. We do not accept connections, but a tracker rejects 0.
    pub port: u16,
    pub uploaded: u64,
    pub downloaded: u64,
    /// Bytes still to fetch. `None` on the first announce of a magnet, before the
    /// metadata says how big the torrent is.
    pub left: Option<u64>,
    pub event: Event,
    /// How many peers to ask for. 0 means "stop announcing".
    pub numwant: u32,
}

impl Announce {
    pub fn new(info_hash: [u8; 20], peer_id: [u8; 20], port: u16) -> Announce {
        Announce {
            info_hash,
            peer_id,
            port,
            uploaded: 0,
            downloaded: 0,
            left: None,
            event: Event::Started,
            numwant: 200,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AnnounceResponse {
    pub peers: Vec<SocketAddr>,
    /// Seconds until the next announce, as the tracker asked.
    pub interval: u64,
    pub min_interval: u64,
    pub seeders: u64,
    pub leechers: u64,
}

impl AnnounceResponse {
    /// The wait before the next announce: the tracker's interval, but never less than
    /// its minimum and never less than a minute (trackers that answer `0` mean
    /// "don't hammer me", not "ask again now").
    pub fn next_wait(&self) -> Duration {
        let secs = self.interval.max(self.min_interval).clamp(60, 3600);
        Duration::from_secs(secs)
    }
}

/// Announce to one tracker URL, whatever scheme it uses.
pub async fn announce(url: &str, req: &Announce) -> Result<AnnounceResponse> {
    if url.starts_with("udp://") {
        announce_udp(url, req).await
    } else if url.starts_with("http://") || url.starts_with("https://") {
        announce_http(url, req).await
    } else if url.starts_with("wss://") || url.starts_with("ws://") {
        // WebSocket trackers (BEP 16) are rare; announcing over TCP is not.
        Err(OdmError::other("websocket trackers are not supported"))
    } else {
        Err(OdmError::other(format!("unknown tracker scheme: {url}")))
    }
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

async fn announce_http(url: &str, req: &Announce) -> Result<AnnounceResponse> {
    let left = req.left.unwrap_or(0);
    let mut full = format!(
        "{}{}info_hash={}&peer_id={}&port={}&uploaded={}&downloaded={}&left={}&numwant={}&compact=1",
        url,
        if url.contains('?') { '&' } else { '?' },
        escape(&req.info_hash),
        escape(&req.peer_id),
        req.port,
        req.uploaded,
        req.downloaded,
        left,
        req.numwant,
    );
    if req.event != Event::None {
        full.push_str("&event=");
        full.push_str(req.event.as_str());
    }
    if req.event == Event::Stopped {
        // No point asking for more peers when saying goodbye.
        full = full.replace(&format!("numwant={}", req.numwant), "numwant=0");
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| OdmError::other(e.to_string()))?;
    let body = client
        .get(&full)
        .header("User-Agent", crate::config::DEFAULT_UA)
        .send()
        .await
        .map_err(|e| OdmError::other(format!("tracker: {e}")))?
        .bytes()
        .await
        .map_err(|e| OdmError::other(format!("tracker: {e}")))?;
    parse_response(&body)
}

fn parse_response(body: &[u8]) -> Result<AnnounceResponse> {
    let val = bencode::decode_one_doc(body)
        .map_err(|e| OdmError::other(format!("tracker answered with bad bencode: {e}")))?;
    if let Some(reason) = val.dict_get_str(b"failure reason") {
        return Err(OdmError::other(format!("tracker refused: {reason}")));
    }
    let mut out = AnnounceResponse {
        interval: val.dict_get_int(b"interval").unwrap_or(1800).max(0) as u64,
        min_interval: val.dict_get_int(b"min interval").unwrap_or(0).max(0) as u64,
        seeders: val.dict_get_int(b"complete").unwrap_or(0).max(0) as u64,
        leechers: val.dict_get_int(b"incomplete").unwrap_or(0).max(0) as u64,
        peers: Vec::new(),
    };
    if let Some(p) = val.dict_get(b"peers") {
        if let Some(compact) = p.as_bytes() {
            for chunk in compact.as_chunks::<6>().0 {
                let ip = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
                let port = u16::from_be_bytes([chunk[4], chunk[5]]);
                if port != 0 {
                    out.peers.push(SocketAddr::from((ip, port)));
                }
            }
        } else if let Some(list) = p.as_list() {
            for item in list {
                let (Some(ip), Some(port)) = (item.dict_get_str(b"ip"), item.dict_get_int(b"port"))
                else {
                    continue;
                };
                if let Ok(ip) = ip.parse::<std::net::IpAddr>() {
                    let port = port as u16;
                    if port != 0 {
                        out.peers.push(SocketAddr::new(ip, port));
                    }
                }
            }
        }
    }
    // BEP 7: IPv6 peers, 18 bytes each.
    if let Some(p) = val.dict_get(b"peers6").and_then(|v| v.as_bytes()) {
        for chunk in p.as_chunks::<18>().0 {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&chunk[..16]);
            let port = u16::from_be_bytes([chunk[16], chunk[17]]);
            if port != 0 {
                out.peers
                    .push(SocketAddr::from((Ipv6Addr::from(octets), port)));
            }
        }
    }
    Ok(out)
}

/// Percent-encode bytes the way a tracker's query string needs them: everything
/// except the RFC 3986 unreserved set, including bytes that are not UTF-8 at all
/// (an info hash is arbitrary binary).
fn escape(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 3);
    for &b in bytes {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                s.push(b as char)
            }
            _ => s.push_str(&format!("%{b:02X}")),
        }
    }
    s
}

// ---------------------------------------------------------------------------
// UDP (BEP 15)
// ---------------------------------------------------------------------------

const UDP_MAGIC: u64 = 0x41727101980;
const ACTION_CONNECT: u32 = 0;
const ACTION_ANNOUNCE: u32 = 1;
const ACTION_ERROR: u32 = 3;

/// One request/response exchange with a UDP tracker.
///
/// The protocol has two round trips: `connect` hands out a one-hour connection id,
/// then `announce` uses it. Both are retried with an exponential backoff, because a
/// dropped UDP packet is routine and must not look like a dead tracker.
async fn announce_udp(url: &str, req: &Announce) -> Result<AnnounceResponse> {
    let addrs = resolve_udp(url).await?;
    let mut last = None;
    for addr in addrs {
        match announce_udp_to(addr, req).await {
            Ok(resp) => return Ok(resp),
            // A tracker name can resolve to several addresses, and one of them may
            // be unreachable or the wrong family. Trying the next is cheaper than
            // losing the tracker.
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| OdmError::other(format!("udp tracker: {url} does not resolve"))))
}

/// One announce to one already-resolved tracker address.
async fn announce_udp_to(addr: SocketAddr, req: &Announce) -> Result<AnnounceResponse> {
    // The socket must be the same family as the tracker: binding `0.0.0.0` and
    // then connecting to an IPv6 address fails with "Address family not supported",
    // which silently lost every IPv6-only tracker name.
    let bind: SocketAddr = if addr.is_ipv4() {
        SocketAddr::from(([0, 0, 0, 0], 0))
    } else {
        SocketAddr::from(([0u16; 8], 0))
    };
    let socket = tokio::net::UdpSocket::bind(bind)
        .await
        .map_err(|e| OdmError::other(format!("udp tracker socket: {e}")))?;
    socket
        .connect(addr)
        .await
        .map_err(|e| OdmError::other(format!("udp tracker connect: {e}")))?;

    let connection_id = udp_connect(&socket).await?;

    let mut payload = Vec::with_capacity(98);
    payload.extend_from_slice(&connection_id.to_be_bytes());
    payload.extend_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
    let mut txid = [0u8; 4];
    rand::Rng::fill(&mut rand::thread_rng(), &mut txid);
    let txid = u32::from_be_bytes(txid);
    payload.extend_from_slice(&txid.to_be_bytes());
    payload.extend_from_slice(&req.info_hash);
    payload.extend_from_slice(&req.peer_id);
    payload.extend_from_slice(&req.downloaded.to_be_bytes());
    payload.extend_from_slice(&req.left.unwrap_or(0).to_be_bytes());
    payload.extend_from_slice(&req.uploaded.to_be_bytes());
    payload.extend_from_slice(&req.event.as_u32().to_be_bytes());
    payload.extend_from_slice(&0u32.to_be_bytes()); // IP: 0 = "the address you see"
    let mut key = [0u8; 4];
    rand::Rng::fill(&mut rand::thread_rng(), &mut key);
    payload.extend_from_slice(&key);
    payload.extend_from_slice(&req.numwant.to_be_bytes());
    payload.extend_from_slice(&req.port.to_be_bytes());

    let resp = udp_exchange(&socket, &payload, txid).await?;
    if resp.len() < 20 {
        return Err(OdmError::other("udp tracker: short announce response"));
    }
    let action = u32::from_be_bytes([resp[0], resp[1], resp[2], resp[3]]);
    if action == ACTION_ERROR {
        let msg = String::from_utf8_lossy(&resp[8..]).into_owned();
        return Err(OdmError::other(format!("tracker refused: {msg}")));
    }
    if action != ACTION_ANNOUNCE {
        return Err(OdmError::other("udp tracker: unexpected action"));
    }
    let interval = u32::from_be_bytes([resp[8], resp[9], resp[10], resp[11]]) as u64;
    let leechers = u32::from_be_bytes([resp[12], resp[13], resp[14], resp[15]]) as u64;
    let seeders = u32::from_be_bytes([resp[16], resp[17], resp[18], resp[19]]) as u64;
    let mut peers = Vec::new();
    for chunk in resp[20..].as_chunks::<6>().0 {
        let ip = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
        let port = u16::from_be_bytes([chunk[4], chunk[5]]);
        if port != 0 {
            peers.push(SocketAddr::from((ip, port)));
        }
    }
    Ok(AnnounceResponse {
        peers,
        interval,
        min_interval: 0,
        seeders,
        leechers,
    })
}

async fn udp_connect(socket: &tokio::net::UdpSocket) -> Result<u64> {
    let mut payload = Vec::with_capacity(16);
    payload.extend_from_slice(&UDP_MAGIC.to_be_bytes());
    payload.extend_from_slice(&ACTION_CONNECT.to_be_bytes());
    let mut txid = [0u8; 4];
    rand::Rng::fill(&mut rand::thread_rng(), &mut txid);
    let txid = u32::from_be_bytes(txid);
    payload.extend_from_slice(&txid.to_be_bytes());

    let resp = udp_exchange(socket, &payload, txid).await?;
    if resp.len() < 16 {
        return Err(OdmError::other("udp tracker: short connect response"));
    }
    let action = u32::from_be_bytes([resp[0], resp[1], resp[2], resp[3]]);
    if action == ACTION_ERROR {
        let msg = String::from_utf8_lossy(&resp[8..]).into_owned();
        return Err(OdmError::other(format!("tracker refused: {msg}")));
    }
    if action != ACTION_CONNECT {
        return Err(OdmError::other("udp tracker: unexpected connect action"));
    }
    Ok(u64::from_be_bytes([
        resp[8], resp[9], resp[10], resp[11], resp[12], resp[13], resp[14], resp[15],
    ]))
}

/// Send a datagram and wait for the reply carrying `txid`, retrying with a doubling
/// timeout. Returns the bytes after the 8-byte `action`/`transaction` header.
async fn udp_exchange(
    socket: &tokio::net::UdpSocket,
    payload: &[u8],
    txid: u32,
) -> Result<Vec<u8>> {
    let mut wait = Duration::from_millis(1500);
    let mut last = None;
    // Three attempts, not four: a dead tracker should cost seconds, not half a
    // minute, because the other tiers wait for this one before the next round.
    for _ in 0..3 {
        socket
            .send(payload)
            .await
            .map_err(|e| OdmError::other(format!("udp tracker: {e}")))?;
        let mut buf = vec![0u8; 1500];
        match tokio::time::timeout(wait, socket.recv(&mut buf)).await {
            Ok(Ok(n)) => {
                buf.truncate(n);
                if buf.len() >= 8 && u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) == txid {
                    return Ok(buf);
                }
                last = Some("udp tracker: transaction id mismatch".to_string());
            }
            Ok(Err(e)) => last = Some(format!("udp tracker: {e}")),
            Err(_) => last = Some("udp tracker: timed out".to_string()),
        }
        wait *= 2;
    }
    Err(OdmError::other(
        last.unwrap_or_else(|| "udp tracker: no answer".into()),
    ))
}

async fn resolve_udp(url: &str) -> Result<Vec<SocketAddr>> {
    let rest = url
        .strip_prefix("udp://")
        .ok_or_else(|| OdmError::other("not a udp tracker"))?;
    // Trim the path and any query: `udp://host:port/announce`.
    let hostport = rest.split('/').next().unwrap_or(rest);
    if let Ok(addr) = hostport.parse::<SocketAddr>() {
        return Ok(vec![addr]);
    }
    let with_port: String = if hostport.contains(':') && !hostport.ends_with(']') {
        hostport.to_string()
    } else {
        // A bare IPv6 literal is bracketed, so its colons are not a port.
        format!("{hostport}:80")
    };
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(with_port.as_str())
        .await
        .map_err(|e| OdmError::other(format!("udp tracker: {e}")))?
        .collect();
    if addrs.is_empty() {
        return Err(OdmError::other(format!(
            "udp tracker: {hostport} does not resolve"
        )));
    }
    // IPv4 first — it is where the large majority of trackers answer — but keep
    // the IPv6 addresses, because some tracker names are IPv6-only.
    let mut ordered: Vec<SocketAddr> = addrs.iter().copied().filter(SocketAddr::is_ipv4).collect();
    ordered.extend(addrs.into_iter().filter(SocketAddr::is_ipv6));
    Ok(ordered)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compact(peers: &[(u8, u8, u8, u8, u16)]) -> Vec<u8> {
        let mut v = Vec::new();
        for p in peers {
            v.extend_from_slice(&[p.0, p.1, p.2, p.3]);
            v.extend_from_slice(&p.4.to_be_bytes());
        }
        v
    }

    #[test]
    fn parses_a_compact_http_answer() {
        let peers = compact(&[(1, 2, 3, 4, 6881), (5, 6, 7, 8, 51413)]);
        let mut body = Vec::new();
        body.extend_from_slice(b"d8:completei5e10:incompletei9e8:intervali1800e5:peers");
        body.extend_from_slice(peers.len().to_string().as_bytes());
        body.push(b':');
        body.extend_from_slice(&peers);
        body.extend_from_slice(b"e");

        let r = parse_response(&body).unwrap();
        assert_eq!(r.peers.len(), 2);
        assert_eq!(r.seeders, 5);
        assert_eq!(r.leechers, 9);
        assert_eq!(r.interval, 1800);
        assert_eq!(r.peers[1].to_string(), "5.6.7.8:51413");
    }

    #[test]
    fn parses_the_dictionary_form_old_trackers_send() {
        let body = b"d5:peersld2:ip9:127.1.2.34:porti6881eeee";
        let r = parse_response(body).unwrap();
        assert_eq!(r.peers, vec!["127.1.2.3:6881".parse().unwrap()]);
    }

    #[test]
    fn a_failure_reason_is_an_error_not_an_empty_swarm() {
        let body = b"d14:failure reason15:no such torrente";
        let err = parse_response(body).unwrap_err();
        assert!(format!("{err}").contains("no such torrent"), "{err}");
    }

    #[test]
    fn parses_ipv6_peers() {
        let mut body = Vec::new();
        body.extend_from_slice(b"d5:peers0:6:peers618:");
        body.extend_from_slice(&[0u8; 15]);
        body.push(1);
        body.extend_from_slice(&6881u16.to_be_bytes());
        body.extend_from_slice(b"e");
        let r = parse_response(&body).unwrap();
        assert_eq!(r.peers, vec!["[::1]:6881".parse().unwrap()]);
    }

    #[test]
    fn info_hashes_are_percent_escaped_as_binary() {
        let hash = [0x00, 0x7f, 0xff, b'a'];
        assert_eq!(escape(&hash), "%00%7F%FFa");
    }

    #[test]
    fn resolves_a_udp_url_without_a_scheme_port() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let addrs = resolve_udp("udp://127.0.0.1:6969/announce").await.unwrap();
            assert_eq!(addrs, vec!["127.0.0.1:6969".parse().unwrap()]);
        });
    }

    #[tokio::test]
    async fn a_udp_tracker_round_trip_works_against_our_own_socket() {
        // A real BEP 15 server, on loopback, so the framing is actually exercised.
        let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            // connect
            let (n, from) = server.recv_from(&mut buf).await.unwrap();
            assert_eq!(n, 16);
            let txid = &buf[12..16];
            let mut reply = Vec::new();
            reply.extend_from_slice(&ACTION_CONNECT.to_be_bytes());
            reply.extend_from_slice(txid);
            reply.extend_from_slice(&0x1122334455667788u64.to_be_bytes());
            server.send_to(&reply, from).await.unwrap();
            // announce
            let (n, from) = server.recv_from(&mut buf).await.unwrap();
            assert_eq!(n, 98);
            let txid = &buf[12..16];
            let mut reply = Vec::new();
            reply.extend_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
            reply.extend_from_slice(txid);
            reply.extend_from_slice(&1800u32.to_be_bytes());
            reply.extend_from_slice(&7u32.to_be_bytes());
            reply.extend_from_slice(&3u32.to_be_bytes());
            reply.extend_from_slice(&compact(&[(9, 8, 7, 6, 6881)]));
            server.send_to(&reply, from).await.unwrap();
        });

        let mut req = Announce::new([1u8; 20], [2u8; 20], 6881);
        req.left = Some(1000);
        let r = announce_udp(&format!("udp://{addr}/announce"), &req)
            .await
            .unwrap();
        assert_eq!(r.interval, 1800);
        assert_eq!(r.seeders, 3);
        assert_eq!(r.leechers, 7);
        assert_eq!(r.peers, vec!["9.8.7.6:6881".parse().unwrap()]);
        handle.await.unwrap();
    }
}
