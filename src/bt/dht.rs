//! Mainline DHT (BEP 5): finding peers for a magnet that lists no working tracker.
//!
//! uTorrent says it plainly when a magnet fails: *"magnet link needs DHT"*. A magnet
//! is only an info hash, and the swarm that has the data is found by asking the DHT's
//! distributed hash table who is close to that hash. Without this, a magnet whose
//! trackers are blocked or dead cannot start at all.
//!
//! This is a *lookup* client, not a routing node: it bootstraps, walks the table
//! towards the info hash, and collects the peer addresses it is given. It is also
//! polite enough to answer a `ping` while it is there.

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;

use crate::bt::bencode::{self, Value};
use crate::error::{OdmError, Result};

/// The routers every client bootstraps from. They are not special — they are just
/// nodes that are always up — but one of them answering is what gets the walk started.
const BOOTSTRAP: &[(&str, u16)] = &[
    ("router.bittorrent.com", 6881),
    ("dht.transmissionbt.com", 6881),
    ("router.utorrent.com", 6881),
    ("dht.libtorrent.org", 25401),
    ("dht.aelitis.com", 6881),
];

const QUERY_TIMEOUT: Duration = Duration::from_millis(1500);
const MAX_NODES: usize = 200;

type OnPeer = Arc<dyn Fn(SocketAddr) + Send + Sync>;

/// Walk the DHT towards `info_hash`, handing every peer found to `on_peer`.
///
/// Returns the number of peers found. A failure to bootstrap is an error only when
/// nothing at all answered; a lookup that finds no peers is a legitimate empty result.
pub async fn lookup(
    info_hash: [u8; 20],
    on_peer: OnPeer,
    cancel: Arc<AtomicBool>,
) -> Result<usize> {
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| OdmError::other(format!("dht socket: {e}")))?;
    let mut id = [0u8; 20];
    rand::Rng::fill(&mut rand::thread_rng(), &mut id);

    let mut nodes: HashMap<SocketAddr, [u8; 20]> = HashMap::new();
    let seeds = bootstrap(&socket, &id).await?;
    for (addr, node) in seeds {
        nodes.insert(addr, node);
    }
    if nodes.is_empty() {
        return Err(OdmError::other("no DHT bootstrap node answered"));
    }

    let mut queried: HashSet<SocketAddr> = HashSet::new();
    let mut peers: HashSet<SocketAddr> = HashSet::new();
    let mut buf = vec![0u8; 4096];

    let mut empty_rounds = 0usize;
    for _ in 0..12 {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let mut batch: Vec<(SocketAddr, [u8; 20])> = nodes
            .iter()
            .filter(|(addr, _)| !queried.contains(*addr))
            .map(|(a, n)| (*a, *n))
            .collect();
        if batch.is_empty() {
            break;
        }
        batch.sort_by_key(|(_, node)| distance(node, &info_hash));
        batch.truncate(8);

        let mut progressed = false;
        for (addr, _) in batch {
            queried.insert(addr);
            let payload = query_get_peers(&id, &info_hash);
            if let Some(v) = rpc(&socket, addr, &payload, &mut buf, &id).await {
                progressed = true;
                if let Some(values) = v.dict_get(b"values").and_then(Value::as_list) {
                    for value in values {
                        if let Some(raw) = value.as_bytes() {
                            for chunk in raw.as_chunks::<6>().0 {
                                let ip = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
                                let port = u16::from_be_bytes([chunk[4], chunk[5]]);
                                if port != 0 && !ip.is_unspecified() && !ip.is_loopback() {
                                    let a = SocketAddr::from((ip, port));
                                    if peers.insert(a) {
                                        on_peer(a);
                                    }
                                }
                            }
                        }
                    }
                }
                for (a, n) in parse_nodes(v.dict_get(b"nodes")) {
                    if nodes.len() < MAX_NODES {
                        nodes.insert(a, n);
                    }
                }
            }
        }
        if !progressed || peers.len() >= 100 {
            // An empty round does not mean the walk is over: nodes can be slow, and
            // one unresponsive batch used to end the whole lookup with zero peers.
            empty_rounds += 1;
            if peers.len() >= 100 || empty_rounds >= 2 {
                break;
            }
        } else {
            empty_rounds = 0;
        }
    }
    Ok(peers.len())
}

async fn bootstrap(socket: &UdpSocket, id: &[u8; 20]) -> Result<Vec<(SocketAddr, [u8; 20])>> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 4096];
    let mut answered = 0usize;
    for (host, port) in BOOTSTRAP {
        let addrs = match tokio::time::timeout(
            Duration::from_secs(3),
            tokio::net::lookup_host((*host, *port)),
        )
        .await
        {
            Ok(Ok(it)) => it.collect::<Vec<_>>(),
            _ => continue,
        };
        for addr in addrs.into_iter().filter(|a| a.is_ipv4()).take(2) {
            let payload = query_find_node(id, id);
            if let Some(v) = rpc(socket, addr, &payload, &mut buf, id).await {
                out.extend(parse_nodes(v.dict_get(b"nodes")));
            }
        }
        if !out.is_empty() {
            // Two routers are plenty to start from, and a single unresponsive router
            // must not end the bootstrap. Stopping at the first that answered used to
            // leave the walk with a thin, sometimes empty, table.
            answered += 1;
            if answered >= 2 {
                break;
            }
        }
    }
    Ok(out)
}

/// One request/response exchange. Returns the `r` dictionary.
async fn rpc(
    socket: &UdpSocket,
    addr: SocketAddr,
    payload: &[u8],
    buf: &mut [u8],
    _id: &[u8; 20],
) -> Option<Value> {
    socket.send_to(payload, addr).await.ok()?;
    let deadline = tokio::time::Instant::now() + QUERY_TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return None;
        }
        let n = match tokio::time::timeout(left, socket.recv_from(buf)).await {
            Ok(Ok((n, from))) if from == addr => n,
            Ok(Ok((n, from))) => {
                // A query from some other node: answer a ping so we are not a black
                // hole, and keep waiting for our answer.
                answer_query(socket, from, &buf[..n]).await;
                continue;
            }
            _ => return None,
        };
        let Ok(v) = bencode::decode_one_doc(&buf[..n]) else {
            continue;
        };
        if v.dict_get_str(b"y") == Some("r") {
            return v.dict_get(b"r").cloned();
        }
        if v.dict_get_str(b"y") == Some("e") {
            return None;
        }
    }
}

/// Reply to a `ping` (and nothing else) so other nodes keep us in their table.
async fn answer_query(socket: &UdpSocket, from: SocketAddr, data: &[u8]) {
    let Ok(v) = bencode::decode_one_doc(data) else {
        return;
    };
    if v.dict_get_str(b"y") != Some("q") || v.dict_get_str(b"q") != Some("ping") {
        return;
    }
    let Some(t) = v.dict_get(b"t").and_then(Value::as_bytes) else {
        return;
    };
    let mut reply = Vec::new();
    reply.extend_from_slice(b"d1:rd2:id20:");
    reply.extend_from_slice(&self_id());
    reply.extend_from_slice(b"e1:t");
    reply.extend_from_slice(t.len().to_string().as_bytes());
    reply.push(b':');
    reply.extend_from_slice(t);
    reply.extend_from_slice(b"1:y1:re");
    let _ = socket.send_to(&reply, from).await;
}

fn self_id() -> [u8; 20] {
    let mut id = [0u8; 20];
    rand::Rng::fill(&mut rand::thread_rng(), &mut id);
    id
}

fn query_get_peers(id: &[u8; 20], info_hash: &[u8; 20]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"d1:ad2:id20:");
    out.extend_from_slice(id);
    out.extend_from_slice(b"9:info_hash20:");
    out.extend_from_slice(info_hash);
    out.extend_from_slice(b"e1:q9:get_peers1:t2:aa1:y1:qe");
    out
}

fn query_find_node(id: &[u8; 20], target: &[u8; 20]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"d1:ad2:id20:");
    out.extend_from_slice(id);
    out.extend_from_slice(b"6:target20:");
    out.extend_from_slice(target);
    out.extend_from_slice(b"e1:q9:find_node1:t2:ab1:y1:qe");
    out
}

fn parse_nodes(v: Option<&Value>) -> Vec<(SocketAddr, [u8; 20])> {
    let Some(raw) = v.and_then(Value::as_bytes) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for chunk in raw.as_chunks::<26>().0 {
        let mut id = [0u8; 20];
        id.copy_from_slice(&chunk[..20]);
        let ip = Ipv4Addr::new(chunk[20], chunk[21], chunk[22], chunk[23]);
        let port = u16::from_be_bytes([chunk[24], chunk[25]]);
        if port != 0 && !ip.is_unspecified() && !ip.is_loopback() {
            out.push((SocketAddr::from((ip, port)), id));
        }
    }
    out
}

/// XOR distance as a 160-bit big-endian number, compared as 20 bytes.
fn distance(a: &[u8; 20], b: &[u8; 20]) -> [u8; 20] {
    let mut d = [0u8; 20];
    for i in 0..20 {
        d[i] = a[i] ^ b[i];
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_contacts_are_twenty_six_bytes() {
        let mut raw = Vec::new();
        raw.extend_from_slice(&[7u8; 20]);
        raw.extend_from_slice(&[1, 2, 3, 4]);
        raw.extend_from_slice(&6881u16.to_be_bytes());
        let v = Item::bytes(raw);
        let nodes = parse_nodes(Some(&v));
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].0.to_string(), "1.2.3.4:6881");
        assert_eq!(nodes[0].1, [7u8; 20]);
    }

    #[test]
    fn a_get_peers_query_is_valid_bencode() {
        let id = [1u8; 20];
        let hash = [2u8; 20];
        let q = query_get_peers(&id, &hash);
        let v = bencode::decode_one_doc(&q).unwrap();
        assert_eq!(v.dict_get_str(b"y"), Some("q"));
        assert_eq!(v.dict_get_str(b"q"), Some("get_peers"));
        assert_eq!(
            v.dict_get(b"a")
                .and_then(|a| a.dict_get(b"info_hash"))
                .and_then(Value::as_bytes),
            Some(&hash[..])
        );
    }

    #[test]
    fn distance_prefers_the_near_node() {
        let target = [0u8; 20];
        let near = {
            let mut n = [0u8; 20];
            n[19] = 1;
            n
        };
        let far = {
            let mut n = [0u8; 20];
            n[0] = 1;
            n
        };
        assert!(distance(&near, &target) < distance(&far, &target));
    }

    /// A tiny helper so the test can build a `Value::Bytes` without importing it.
    struct Item;
    impl Item {
        fn bytes(b: Vec<u8>) -> Value {
            Value::Bytes(b)
        }
    }
}
