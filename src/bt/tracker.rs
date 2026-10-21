//! HTTP tracker announce: GET <announce>?info_hash=...&peer_id=...&port=...&uploaded=0
//! &downloaded=0&left=<bytes>&compact=1

use crate::bt::torrent::Torrent;
use crate::error::{OdmError, Result};

#[derive(Debug, Clone)]
pub struct Peer {
    pub ip: String,
    pub port: u16,
}

pub async fn announce(
    torrent: &Torrent,
    peer_id: &[u8; 20],
    port: u16,
    left: u64,
) -> Result<Vec<Peer>> {
    announce_at(&torrent.announce, &torrent.info_hash, peer_id, port, left).await
}

/// Announce to one tracker URL.
///
/// A magnet link has no torrent file yet, so the hash is all we have to go on —
/// which is exactly what a tracker wants.
pub async fn announce_at(
    url: &str,
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
    port: u16,
    left: u64,
) -> Result<Vec<Peer>> {
    let url = format!(
        "{}?info_hash={}&peer_id={}&port={}&uploaded=0&downloaded=0&left={}&compact=1&event=started",
        url,
        urlencode_bytes(info_hash),
        urlencode_bytes(peer_id),
        port,
        left,
    );
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| OdmError::other(e.to_string()))?;
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| OdmError::other(e.to_string()))?;
    let body = resp
        .bytes()
        .await
        .map_err(|e| OdmError::other(e.to_string()))?;
    // compact peers: 6 bytes each (4 ip + 2 port)
    let val = crate::bt::bencode::decode_one_doc(&body)?;
    // peers may be in "peers" (compact) or "peers" list
    let mut peers = Vec::new();
    if let Some(p) = val.dict_get(b"peers") {
        if let Some(b) = p.as_bytes() {
            for chunk in b.chunks(6) {
                if chunk.len() == 6 {
                    peers.push(Peer {
                        ip: format!("{}.{}.{}.{}", chunk[0], chunk[1], chunk[2], chunk[3]),
                        port: u16::from_be_bytes([chunk[4], chunk[5]]),
                    });
                }
            }
        } else if let Some(list) = p.as_list() {
            for item in list {
                if let (Some(ip), Some(port)) =
                    (item.dict_get_str(b"ip"), item.dict_get_int(b"port"))
                {
                    peers.push(Peer {
                        ip: ip.to_string(),
                        port: port as u16,
                    });
                }
            }
        }
    }
    Ok(peers)
}

fn urlencode_bytes(b: &[u8]) -> String {
    let mut s = String::new();
    for &c in b {
        // percent-encode everything except unreserved
        if c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c == b'.' || c == b'~' {
            s.push(c as char);
        } else {
            s.push_str(&format!("%{:02X}", c));
        }
    }
    s
}
