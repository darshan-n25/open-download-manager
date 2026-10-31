//! Bencode (BitTorrent's serialization format): integers `i<n>e`, byte strings
//! `<len>:<bytes>`, lists `l...e`, dictionaries `d...e`. No other types exist.

use crate::error::{OdmError, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Dict(Vec<(Vec<u8>, Value)>),
}

impl Value {
    pub fn as_int(&self) -> Option<i64> {
        if let Value::Int(n) = self {
            Some(*n)
        } else {
            None
        }
    }
    pub fn as_bytes(&self) -> Option<&[u8]> {
        if let Value::Bytes(b) = self {
            Some(b)
        } else {
            None
        }
    }
    pub fn as_list(&self) -> Option<&[Value]> {
        if let Value::List(v) = self {
            Some(v)
        } else {
            None
        }
    }
    pub fn as_dict(&self) -> Option<&[(Vec<u8>, Value)]> {
        if let Value::Dict(v) = self {
            Some(v)
        } else {
            None
        }
    }
    pub fn dict_get(&self, key: &[u8]) -> Option<&Value> {
        self.as_dict()?
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }
    pub fn dict_get_str(&self, key: &[u8]) -> Option<&str> {
        std::str::from_utf8(self.dict_get(key)?.as_bytes()?).ok()
    }
    pub fn dict_get_int(&self, key: &[u8]) -> Option<i64> {
        self.dict_get(key)?.as_int()
    }
}

/// Decode the first value in `data`, returning it and the number of bytes consumed.
pub fn decode(data: &[u8]) -> Result<(Value, usize)> {
    let (v, rest) = decode_one(data)?;
    let consumed = data.len() - rest.len();
    Ok((v, consumed))
}

/// Decode a complete document: exactly one value, no trailing bytes.
pub fn decode_one_doc(data: &[u8]) -> Result<Value> {
    let (v, consumed) = decode(data)?;
    if consumed != data.len() {
        return Err(OdmError::other("trailing bytes after bencode value"));
    }
    Ok(v)
}

/// The byte range of the value stored under `key` in the top-level dictionary
/// `data`, as `(start, end)` into `data`.
///
/// This exists because a torrent's info hash is the SHA-1 of the *encoded* info
/// dictionary exactly as it appeared in the file. Decoding and re-encoding it is
/// only equivalent when the dictionary's keys were already in canonical order;
/// a file written by a tool that did not sort them would otherwise hash to a
/// different value, and no tracker or peer would recognise the torrent.
pub fn dict_value_span(data: &[u8], key: &[u8]) -> Option<(usize, usize)> {
    if data.first() != Some(&b'd') {
        return None;
    }
    let mut rest = &data[1..];
    while !rest.is_empty() && rest[0] != b'e' {
        let (k, after_key) = decode_one(rest).ok()?;
        let key_bytes = k.as_bytes()?;
        let value_start = data.len() - after_key.len();
        let (_, after_value) = decode_one(after_key).ok()?;
        let value_end = data.len() - after_value.len();
        if key_bytes == key {
            return Some((value_start, value_end));
        }
        rest = after_value;
    }
    None
}

fn decode_one(data: &[u8]) -> Result<(Value, &[u8])> {
    if data.is_empty() {
        return Err(OdmError::other("unexpected end of bencode"));
    }
    match data[0] {
        b'i' => {
            let end = data
                .iter()
                .position(|&b| b == b'e')
                .ok_or_else(|| OdmError::other("unterminated integer"))?;
            let n: i64 = std::str::from_utf8(&data[1..end])
                .map_err(|_| OdmError::other("bad integer"))?
                .parse()
                .map_err(|_| OdmError::other("bad integer"))?;
            Ok((Value::Int(n), &data[end + 1..]))
        }
        b'l' => {
            let mut items = Vec::new();
            let mut rest = &data[1..];
            while !rest.is_empty() && rest[0] != b'e' {
                let (v, r) = decode_one(rest)?;
                items.push(v);
                rest = r;
            }
            if rest.is_empty() {
                return Err(OdmError::other("unterminated list"));
            }
            Ok((Value::List(items), &rest[1..]))
        }
        b'd' => {
            let mut items = Vec::new();
            let mut rest = &data[1..];
            while !rest.is_empty() && rest[0] != b'e' {
                let (k, r) = decode_one(rest)?;
                let key = k
                    .as_bytes()
                    .ok_or_else(|| OdmError::other("dict key not a string"))?
                    .to_vec();
                let (v, r2) = decode_one(r)?;
                items.push((key, v));
                rest = r2;
            }
            if rest.is_empty() {
                return Err(OdmError::other("unterminated dict"));
            }
            Ok((Value::Dict(items), &rest[1..]))
        }
        b'0'..=b'9' => {
            // read length
            let colon = data
                .iter()
                .position(|&b| b == b':')
                .ok_or_else(|| OdmError::other("missing colon in string"))?;
            let len: usize = std::str::from_utf8(&data[..colon])
                .map_err(|_| OdmError::other("bad length"))?
                .parse()
                .map_err(|_| OdmError::other("bad length"))?;
            let start = colon + 1;
            let end = start + len;
            if end > data.len() {
                return Err(OdmError::other("string too short"));
            }
            Ok((Value::Bytes(data[start..end].to_vec()), &data[end..]))
        }
        _ => Err(OdmError::other("invalid bencode prefix")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int() {
        let (v, n) = decode(b"i42e").unwrap();
        assert_eq!(v, Value::Int(42));
        assert_eq!(n, 4);
        assert_eq!(decode_one_doc(b"i-3e").unwrap(), Value::Int(-3));
    }

    #[test]
    fn bytes() {
        let (v, n) = decode(b"4:spam").unwrap();
        assert_eq!(v, Value::Bytes(b"spam".to_vec()));
        assert_eq!(n, 6);
    }

    #[test]
    fn list() {
        let v = decode_one_doc(b"l4:spam4:eggse").unwrap();
        assert_eq!(
            v,
            Value::List(vec![
                Value::Bytes(b"spam".to_vec()),
                Value::Bytes(b"eggs".to_vec())
            ])
        );
    }

    #[test]
    fn dict() {
        let v = decode_one_doc(b"d3:cow3:moo4:spam4:eggse").unwrap();
        let d = v.as_dict().unwrap();
        assert_eq!(d[0].0, b"cow");
        assert_eq!(d[0].1, Value::Bytes(b"moo".to_vec()));
    }

    #[test]
    fn nested() {
        let v = decode_one_doc(b"d5:filesld6:lengthi10eeee").unwrap();
        assert!(v.dict_get(b"files").is_some());
    }

    #[test]
    fn rejects_trailing() {
        assert!(decode_one_doc(b"i1ei2e").is_err());
    }

    #[test]
    fn empty_string() {
        let (v, n) = decode(b"0:").unwrap();
        assert_eq!(v, Value::Bytes(vec![]));
        assert_eq!(n, 2);
    }

    #[test]
    fn a_value_span_points_at_the_original_bytes() {
        // Deliberately out of key order: the span must be of the bytes as they
        // are, not of a canonical re-encoding.
        let data = b"d4:infod6:lengthi7e4:name4:teste8:announce3:urle";
        let (start, end) = dict_value_span(data, b"info").unwrap();
        assert_eq!(&data[start..end], b"d6:lengthi7e4:name4:teste");
        assert_eq!(dict_value_span(data, b"missing"), None);
        assert_eq!(dict_value_span(b"l1:ae", b"info"), None);
    }
}
