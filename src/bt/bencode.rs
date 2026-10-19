//! Bencode (BitTorrent's serialization format): integers `i<n>e`, byte strings
//! `<len>:<bytes>`, lists `l...e`, dictionaries `d...e`. No other types exist.

use crate::error::{Result, RiptideError};

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
        return Err(RiptideError::other("trailing bytes after bencode value"));
    }
    Ok(v)
}

fn decode_one(data: &[u8]) -> Result<(Value, &[u8])> {
    if data.is_empty() {
        return Err(RiptideError::other("unexpected end of bencode"));
    }
    match data[0] {
        b'i' => {
            let end = data
                .iter()
                .position(|&b| b == b'e')
                .ok_or_else(|| RiptideError::other("unterminated integer"))?;
            let n: i64 = std::str::from_utf8(&data[1..end])
                .map_err(|_| RiptideError::other("bad integer"))?
                .parse()
                .map_err(|_| RiptideError::other("bad integer"))?;
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
                return Err(RiptideError::other("unterminated list"));
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
                    .ok_or_else(|| RiptideError::other("dict key not a string"))?
                    .to_vec();
                let (v, r2) = decode_one(r)?;
                items.push((key, v));
                rest = r2;
            }
            if rest.is_empty() {
                return Err(RiptideError::other("unterminated dict"));
            }
            Ok((Value::Dict(items), &rest[1..]))
        }
        b'0'..=b'9' => {
            // read length
            let colon = data
                .iter()
                .position(|&b| b == b':')
                .ok_or_else(|| RiptideError::other("missing colon in string"))?;
            let len: usize = std::str::from_utf8(&data[..colon])
                .map_err(|_| RiptideError::other("bad length"))?
                .parse()
                .map_err(|_| RiptideError::other("bad length"))?;
            let start = colon + 1;
            let end = start + len;
            if end > data.len() {
                return Err(RiptideError::other("string too short"));
            }
            Ok((Value::Bytes(data[start..end].to_vec()), &data[end..]))
        }
        _ => Err(RiptideError::other("invalid bencode prefix")),
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
}
