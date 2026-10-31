//! Magnet links: `magnet:?xt=urn:btih:<hash>&dn=<name>&tr=<tracker>`.
//!
//! A magnet carries no file metadata — only the info hash, a name and the
//! trackers. The piece hashes have to be fetched from the swarm itself, which
//! is what [`crate::bt::metadata`] does.

use crate::error::{OdmError, Result};

#[derive(Clone, Debug)]
pub struct Magnet {
    /// The SHA-1 of the torrent's info dictionary.
    pub info_hash: [u8; 20],
    /// The suggested file name, if the link carried one.
    pub name: Option<String>,
    /// The trackers, in the order the link listed them.
    pub trackers: Vec<String>,
    /// The size the link advertises (`xl`), if it has one. Trackers want it in the
    /// announce: a client that reports 0 bytes left is a seeder, and is treated as
    /// one when peers are handed out.
    pub length: Option<u64>,
}

impl Magnet {
    /// Parse a magnet link.
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        let rest = text
            .strip_prefix("magnet:")
            .ok_or_else(|| OdmError::other("not a magnet link"))?;
        let rest = rest
            .strip_prefix('?')
            .ok_or_else(|| OdmError::other("not a magnet link"))?;

        let mut info_hash: Option<[u8; 20]> = None;
        let mut name = None;
        let mut trackers = Vec::new();
        let mut length = None;

        for part in rest.split('&') {
            let Some((key, value)) = part.split_once('=') else {
                continue;
            };
            match key {
                "xt" => {
                    // urn:btih:<hash> — the only exact-topic scheme we understand.
                    if let Some(hash) = value.strip_prefix("urn:btih:") {
                        info_hash = Some(decode_info_hash(hash)?);
                    }
                }
                "dn" => {
                    name = Some(percent_decode(value));
                }
                "tr" => {
                    let t = percent_decode(value);
                    if !t.is_empty() {
                        trackers.push(t);
                    }
                }
                // `xl` is a hint, not a promise: a wrong one only affects what the
                // tracker is told, never how bytes are counted.
                "xl" => length = value.parse::<u64>().ok().filter(|n| *n > 0),
                _ => {}
            }
        }

        let info_hash =
            info_hash.ok_or_else(|| OdmError::other("the magnet link has no btih topic"))?;
        Ok(Magnet {
            info_hash,
            name,
            trackers,
            length,
        })
    }

    /// The hash in the hexadecimal form a tracker wants.
    pub fn hex(&self) -> String {
        hex::encode(self.info_hash)
    }
}

/// A BitTorrent info hash is 20 bytes, written either as 40 hexadecimal digits
/// or as 32 base32 digits.
fn decode_info_hash(text: &str) -> Result<[u8; 20]> {
    let text = percent_decode(text);
    if text.len() == 40 {
        let mut out = [0u8; 20];
        hex::decode_to_slice(&text, &mut out)
            .map_err(|e| OdmError::other(format!("bad info hash: {e}")))?;
        return Ok(out);
    }
    if text.len() == 32 {
        let bytes = base32_decode(&text.to_ascii_uppercase())
            .ok_or_else(|| OdmError::other("bad base32 info hash"))?;
        if bytes.len() != 20 {
            return Err(OdmError::other("bad base32 info hash"));
        }
        let mut out = [0u8; 20];
        out.copy_from_slice(&bytes);
        return Ok(out);
    }
    Err(OdmError::other(
        "an info hash is 40 hex or 32 base32 digits",
    ))
}

/// RFC 4648 base32, without padding — the form BitTorrent magnets use.
fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for c in text.chars() {
        let v = match c {
            'A'..='Z' => c as u32 - 'A' as u32,
            '2'..='7' => c as u32 - '2' as u32 + 26,
            _ => return None,
        };
        buffer = (buffer << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

/// The percent-decoding a magnet's parameters need.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hex_hash_is_read() {
        let m = Magnet::parse(
            "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=movie&tr=http://t/announce",
        )
        .unwrap();
        assert_eq!(m.hex(), "0123456789abcdef0123456789abcdef01234567");
        assert_eq!(m.name.as_deref(), Some("movie"));
        assert_eq!(m.trackers, vec!["http://t/announce"]);
    }

    #[test]
    fn a_base32_hash_is_read() {
        // The same 20 bytes, base32.
        let m = Magnet::parse("magnet:?xt=urn:btih:AERSCZ5NE5HGQCTNBI3Z5H2K4T7QNTNU").unwrap();
        assert_eq!(m.info_hash.len(), 20);
    }

    #[test]
    fn a_link_without_a_hash_is_refused() {
        assert!(Magnet::parse("magnet:?dn=x").is_err());
        assert!(Magnet::parse("http://example.com/x.torrent").is_err());
    }

    #[test]
    fn several_trackers_are_kept() {
        let m =
            Magnet::parse("magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&tr=a&tr=b")
                .unwrap();
        assert_eq!(m.trackers, vec!["a", "b"]);
    }

    #[test]
    fn the_advertised_size_is_kept_when_it_is_usable() {
        let m = Magnet::parse(
            "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&xl=2330554994",
        )
        .unwrap();
        assert_eq!(m.length, Some(2330554994));

        // Junk or zero is not a size; announcing it would be worse than not knowing.
        for bad in ["xl=0", "xl=-5", "xl=big"] {
            let m = Magnet::parse(&format!(
                "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&{bad}"
            ))
            .unwrap();
            assert_eq!(m.length, None, "{bad}");
        }
    }
}
