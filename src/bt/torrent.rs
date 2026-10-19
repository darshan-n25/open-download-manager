//! Parsing of a `.torrent` metainfo file into a structured `Torrent`.

use crate::bt::bencode::{self, Value};
use crate::error::{Result, RiptideError};
use sha1::{Digest, Sha1};

#[derive(Debug, Clone)]
pub struct Torrent {
    pub announce: String,
    pub announce_list: Vec<String>,
    pub name: String,
    pub piece_length: u64,
    pub pieces: Vec<[u8; 20]>, // SHA-1 per piece
    pub length: u64,           // single-file torrent
    pub info_hash: [u8; 20],
}

impl Torrent {
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let root = bencode::decode_one_doc(data)?;
        let info = root
            .dict_get(b"info")
            .ok_or_else(|| RiptideError::other("missing info dict"))?;
        let info_bytes = reencode(info)?;
        let mut h = Sha1::new();
        h.update(&info_bytes);
        let info_hash: [u8; 20] = h.finalize().into();

        let name = info
            .dict_get_str(b"name")
            .ok_or_else(|| RiptideError::other("missing name"))?
            .to_string();
        let piece_length =
            info.dict_get_int(b"piece length")
                .ok_or_else(|| RiptideError::other("missing piece length"))? as u64;
        let pieces_raw = info
            .dict_get(b"pieces")
            .and_then(|v| v.as_bytes())
            .ok_or_else(|| RiptideError::other("missing pieces"))?;
        if pieces_raw.len() % 20 != 0 {
            return Err(RiptideError::other("pieces length not multiple of 20"));
        }
        let mut pieces = Vec::with_capacity(pieces_raw.len() / 20);
        for chunk in pieces_raw.chunks(20) {
            let mut h = [0u8; 20];
            h.copy_from_slice(chunk);
            pieces.push(h);
        }
        let length = info
            .dict_get_int(b"length")
            .ok_or_else(|| RiptideError::other("multi-file torrents not supported yet"))?
            as u64;

        let announce = root.dict_get_str(b"announce").unwrap_or("").to_string();
        let mut announce_list = Vec::new();
        if let Some(al) = root.dict_get(b"announce-list") {
            if let Some(outer) = al.as_list() {
                for tier in outer {
                    if let Some(inner) = tier.as_list() {
                        for u in inner {
                            if let Some(s) = u.as_bytes() {
                                if let Ok(s) = std::str::from_utf8(s) {
                                    announce_list.push(s.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
        if announce_list.is_empty() && !announce.is_empty() {
            announce_list.push(announce.clone());
        }

        Ok(Torrent {
            announce,
            announce_list,
            name,
            piece_length,
            pieces,
            length,
            info_hash,
        })
    }

    pub fn num_pieces(&self) -> usize {
        self.pieces.len()
    }

    pub fn piece_size(&self, index: usize) -> u64 {
        if index + 1 == self.pieces.len() {
            let rem = self.length % self.piece_length;
            if rem == 0 {
                self.piece_length
            } else {
                rem
            }
        } else {
            self.piece_length
        }
    }
}

/// Re-encode a bencode value to its canonical bytes (for info-hash computation).
fn reencode(v: &Value) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    write_value(&mut out, v);
    Ok(out)
}

fn write_value(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Int(n) => {
            out.push(b'i');
            out.extend_from_slice(n.to_string().as_bytes());
            out.push(b'e');
        }
        Value::Bytes(b) => {
            out.extend_from_slice(b.len().to_string().as_bytes());
            out.push(b':');
            out.extend_from_slice(b);
        }
        Value::List(items) => {
            out.push(b'l');
            for x in items {
                write_value(out, x);
            }
            out.push(b'e');
        }
        Value::Dict(items) => {
            out.push(b'd');
            for (k, val) in items {
                write_value(out, &Value::Bytes(k.clone()));
                write_value(out, val);
            }
            out.push(b'e');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Minimal single-file torrent built by hand.
    fn sample() -> Vec<u8> {
        // info dict with name, piece length 4, pieces = one 20-byte hash, length 7
        // rebuild cleanly
        let mut buf = Vec::new();
        buf.extend_from_slice(b"d6:lengthi7e4:name4:test12:piece lengthi4e6:pieces20:");
        buf.extend_from_slice(&[b'a'; 20]);
        buf.push(b'e');
        let mut root = Vec::new();
        root.extend_from_slice(b"d8:announce27:http://tracker.example/announce4:infod");
        // re-embed info dict
        let ann = b"http://tracker.example/announce";
        let mut r = Vec::new();
        r.extend_from_slice(b"d8:announce");
        r.extend_from_slice(ann.len().to_string().as_bytes());
        r.push(b':');
        r.extend_from_slice(ann);
        r.extend_from_slice(b"4:info");
        r.extend_from_slice(&buf);
        r.push(b'e');
        r
    }

    #[test]
    fn parses_basic() {
        let data = sample();
        let t = Torrent::from_bytes(&data).unwrap();
        assert_eq!(t.name, "test");
        assert_eq!(t.piece_length, 4);
        assert_eq!(t.length, 7);
        assert_eq!(t.num_pieces(), 1);
        assert_eq!(t.piece_size(0), 3); // 7 % 4 = 3
        assert_eq!(t.announce, "http://tracker.example/announce");
    }

    #[test]
    fn rejects_multifile() {
        // info dict with no `length` key -> rejected
        let ann = b"http://tracker.example/announce";
        let mut info = Vec::new();
        info.extend_from_slice(b"d4:name4:test12:piece lengthi4e6:pieces20:");
        info.extend_from_slice(&[b'a'; 20]);
        info.push(b'e');
        let mut r = Vec::new();
        r.extend_from_slice(b"d8:announce");
        r.extend_from_slice(ann.len().to_string().as_bytes());
        r.push(b':');
        r.extend_from_slice(ann);
        r.extend_from_slice(b"4:info");
        r.extend_from_slice(&info);
        r.push(b'e');
        let err = Torrent::from_bytes(&r).unwrap_err();
        let s = format!("{}", err);
        assert!(s.contains("length") || s.contains("multi"), "{}", s);
    }

    #[test]
    fn info_hash_stable() {
        let data = sample();
        let t1 = Torrent::from_bytes(&data).unwrap();
        let t2 = Torrent::from_bytes(&data).unwrap();
        assert_eq!(t1.info_hash, t2.info_hash);
    }
}
