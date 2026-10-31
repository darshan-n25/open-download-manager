//! Parsing of a `.torrent` metainfo file into a structured `Torrent`.
//!
//! A torrent describes content two ways. A *single-file* torrent carries `length`;
//! a *multi-file* torrent carries a `files` list of `{length, path}` entries. Both
//! are the same thing underneath — one address space of `length` bytes cut into
//! `piece length` pieces — so [`Torrent`] flattens both into one file table with
//! absolute offsets, and every later stage (picking, hashing, writing, streaming)
//! works on torrent offsets rather than on files.

use std::path::{Component, PathBuf};

use crate::bt::bencode::{self, Value};
use crate::error::{OdmError, Result};
use crate::util::sanitize_filename;
use sha1::{Digest, Sha1};

/// One file inside a torrent, with its place in the torrent's address space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// Path components relative to the torrent's name, already sanitised.
    pub path: Vec<String>,
    /// Length in bytes.
    pub length: u64,
    /// Offset of the file's first byte within the torrent's address space.
    pub offset: u64,
}

impl FileEntry {
    /// The path relative to the download directory: `name` first for a multi-file
    /// torrent, nothing for a single-file one.
    pub fn relative_path(&self) -> PathBuf {
        let mut p = PathBuf::new();
        for c in &self.path {
            p.push(c);
        }
        p
    }

    pub fn display_name(&self) -> String {
        self.path.join("/")
    }
}

#[derive(Debug, Clone)]
pub struct Torrent {
    pub announce: String,
    /// Trackers flattened, in the order the torrent listed them.
    pub announce_list: Vec<String>,
    /// Trackers grouped into BEP 12 tiers. The first tier that answers is used.
    pub tiers: Vec<Vec<String>>,
    pub name: String,
    pub piece_length: u64,
    pub pieces: Vec<[u8; 20]>, // SHA-1 per piece
    /// Total size of all files.
    pub length: u64,
    /// The files, with their offsets. Always at least one, even for a single-file
    /// torrent: that is what makes the rest of the engine uniform.
    pub files: Vec<FileEntry>,
    pub info_hash: [u8; 20],
    /// The canonical encoding of the `info` dictionary — what a magnet has to
    /// download, and what a `.torrent` file has to embed to be valid.
    pub info_bytes: Vec<u8>,
    /// BEP 27: the tracker must not be supplemented by DHT or peer exchange.
    pub private: bool,
}

impl Torrent {
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let root = bencode::decode_one_doc(data)?;
        // Hash the info dictionary exactly as it was written. Re-encoding it is
        // only equivalent for canonical bencode, and a torrent made by a tool
        // that did not sort the keys would otherwise get the wrong info hash —
        // which is the difference between a swarm and "no peers".
        let (start, end) = bencode::dict_value_span(data, b"info")
            .ok_or_else(|| OdmError::other("missing info dict"))?;
        let info_bytes = data[start..end].to_vec();
        let info = bencode::decode_one_doc(&info_bytes)?;
        let info_hash = hash_info(&info_bytes);

        let mut torrent = Torrent::from_info(&info, info_bytes, info_hash)?;

        torrent.announce = root.dict_get_str(b"announce").unwrap_or("").to_string();
        let mut tiers: Vec<Vec<String>> = Vec::new();
        if let Some(al) = root.dict_get(b"announce-list") {
            if let Some(outer) = al.as_list() {
                for tier in outer {
                    let mut urls = Vec::new();
                    if let Some(inner) = tier.as_list() {
                        for u in inner {
                            if let Some(s) = u.as_bytes().and_then(|b| std::str::from_utf8(b).ok())
                            {
                                if !s.is_empty() {
                                    urls.push(s.to_string());
                                }
                            }
                        }
                    }
                    if !urls.is_empty() {
                        tiers.push(urls);
                    }
                }
            }
        }
        if tiers.is_empty() && !torrent.announce.is_empty() {
            tiers.push(vec![torrent.announce.clone()]);
        }
        torrent.announce_list = tiers.iter().flatten().cloned().collect();
        torrent.tiers = tiers;

        Ok(torrent)
    }

    /// Build a torrent from an info dictionary — the part of a `.torrent` file
    /// that describes the content.
    fn from_info(info: &Value, info_bytes: Vec<u8>, info_hash: [u8; 20]) -> Result<Self> {
        let name = info
            .dict_get_str(b"name")
            .ok_or_else(|| OdmError::other("missing name"))?
            .to_string();
        let piece_length = info
            .dict_get_int(b"piece length")
            .ok_or_else(|| OdmError::other("missing piece length"))?;
        if piece_length <= 0 {
            return Err(OdmError::other("piece length must be positive"));
        }
        let piece_length = piece_length as u64;
        let pieces_raw = info
            .dict_get(b"pieces")
            .and_then(|v| v.as_bytes())
            .ok_or_else(|| OdmError::other("missing pieces"))?;
        if pieces_raw.len() % 20 != 0 || pieces_raw.is_empty() {
            return Err(OdmError::other(
                "pieces length not a positive multiple of 20",
            ));
        }
        let mut pieces = Vec::with_capacity(pieces_raw.len() / 20);
        for chunk in pieces_raw.chunks(20) {
            let mut h = [0u8; 20];
            h.copy_from_slice(chunk);
            pieces.push(h);
        }

        let private = info.dict_get_int(b"private").unwrap_or(0) == 1;
        let root_name = sanitize_component(&name);

        let files = if let Some(list) = info.dict_get(b"files").and_then(Value::as_list) {
            if list.is_empty() {
                return Err(OdmError::other("the torrent lists no files"));
            }
            let mut files = Vec::with_capacity(list.len());
            let mut offset = 0u64;
            for f in list {
                let length = f
                    .dict_get_int(b"length")
                    .ok_or_else(|| OdmError::other("a file entry has no length"))?;
                if length < 0 {
                    return Err(OdmError::other("a file entry has a negative length"));
                }
                let path = f
                    .dict_get(b"path")
                    .and_then(Value::as_list)
                    .ok_or_else(|| OdmError::other("a file entry has no path"))?;
                let mut components = Vec::with_capacity(path.len() + 1);
                components.push(root_name.clone());
                for c in path {
                    let raw = c
                        .as_bytes()
                        .and_then(|b| std::str::from_utf8(b).ok())
                        .ok_or_else(|| OdmError::other("a file path is not UTF-8"))?;
                    components.push(sanitize_component(raw));
                }
                files.push(FileEntry {
                    path: components,
                    length: length as u64,
                    offset,
                });
                offset += length as u64;
            }
            files
        } else {
            let length = info
                .dict_get_int(b"length")
                .ok_or_else(|| OdmError::other("missing length and files list"))?;
            if length < 0 {
                return Err(OdmError::other("negative length"));
            }
            vec![FileEntry {
                path: vec![root_name.clone()],
                length: length as u64,
                offset: 0,
            }]
        };

        let length: u64 = files.iter().map(|f| f.length).sum();
        let expected_pieces = length.div_ceil(piece_length) as usize;
        if pieces.len() != expected_pieces {
            // A mismatch means the metainfo is truncated or the lengths were edited;
            // downloading it would hash-fail on every piece near the end.
            return Err(OdmError::other(format!(
                "the torrent lists {} pieces but its length needs {}",
                pieces.len(),
                expected_pieces
            )));
        }

        Ok(Torrent {
            announce: String::new(),
            announce_list: Vec::new(),
            tiers: Vec::new(),
            name: root_name,
            piece_length,
            pieces,
            length,
            files,
            info_hash,
            info_bytes,
            private,
        })
    }

    /// Read the metadata a peer sent us, and check it against the hash the
    /// magnet link promised.
    ///
    /// The metadata is the torrent's *info dictionary*, not the whole `.torrent`
    /// file: that is what the extension protocol exchanges. A peer can send any
    /// metadata it likes, so the hash is the only thing that says whether it is
    /// the torrent the magnet asked for.
    pub fn from_metadata(data: &[u8], expected: [u8; 20]) -> Result<Self> {
        // The peer sent the info dictionary itself, so these bytes *are* the
        // thing the magnet's hash names. Do not re-encode them first.
        let info = bencode::decode_one_doc(data)?;
        let info_hash = hash_info(data);
        if info_hash != expected {
            return Err(OdmError::other(
                "the metadata a peer sent is for a different torrent",
            ));
        }
        Torrent::from_info(&info, data.to_vec(), info_hash)
    }

    pub fn num_pieces(&self) -> usize {
        self.pieces.len()
    }

    pub fn num_files(&self) -> usize {
        self.files.len()
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

    /// The byte offset of a piece within the torrent's address space.
    pub fn piece_offset(&self, index: usize) -> u64 {
        index as u64 * self.piece_length
    }

    /// The files a byte range touches, as `(file index, offset in file, length)`.
    pub fn segments(&self, offset: u64, len: u64) -> Vec<(usize, u64, u64)> {
        let mut out = Vec::new();
        let mut at = offset;
        let end = offset + len;
        for (i, f) in self.files.iter().enumerate() {
            let fend = f.offset + f.length;
            if fend <= at || f.length == 0 {
                continue;
            }
            if f.offset >= end {
                break;
            }
            let start = at.max(f.offset);
            let stop = end.min(fend);
            if stop > start {
                out.push((i, start - f.offset, stop - start));
            }
            at = stop;
        }
        out
    }

    /// Which file holds the majority of a piece: the file of its first byte.
    pub fn file_of_piece(&self, index: usize) -> usize {
        let off = self.piece_offset(index);
        self.files
            .iter()
            .position(|f| off >= f.offset && off < f.offset + f.length)
            .unwrap_or_else(|| self.files.len() - 1)
    }

    /// The pieces covering a byte range, as a half-open range.
    pub fn pieces_for_range(&self, offset: u64, len: u64) -> std::ops::Range<usize> {
        if self.piece_length == 0 {
            return 0..0;
        }
        let first = (offset / self.piece_length) as usize;
        let last = ((offset + len).saturating_sub(1) / self.piece_length) as usize;
        let end = (last + 1).min(self.pieces.len());
        first.min(end)..end
    }

    /// Rebuild a `.torrent` file body from the stored info dictionary.
    pub fn metainfo(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(b'd');
        if !self.announce.is_empty() {
            write_bytes(&mut out, b"announce", self.announce.as_bytes());
        }
        if !self.tiers.is_empty() {
            out.extend_from_slice(b"13:announce-list");
            out.push(b'l');
            for tier in &self.tiers {
                out.push(b'l');
                for url in tier {
                    out.extend_from_slice(url.len().to_string().as_bytes());
                    out.push(b':');
                    out.extend_from_slice(url.as_bytes());
                }
                out.push(b'e');
            }
            out.push(b'e');
        }
        out.extend_from_slice(b"4:info");
        out.extend_from_slice(&self.info_bytes);
        out.push(b'e');
        out
    }

    /// A safe file name for the torrent, without separators.
    pub fn file_name(&self) -> &str {
        self.files
            .first()
            .map(|f| f.path.last().map(String::as_str).unwrap_or(&self.name))
            .unwrap_or(&self.name)
    }
}

fn write_bytes(out: &mut Vec<u8>, key: &[u8], value: &[u8]) {
    out.extend_from_slice(key.len().to_string().as_bytes());
    out.push(b':');
    out.extend_from_slice(key);
    out.extend_from_slice(value.len().to_string().as_bytes());
    out.push(b':');
    out.extend_from_slice(value);
}

/// One path component, made safe on every platform.
///
/// `..` and absolute components are dropped rather than sanitised: a torrent that
/// names `../../.ssh/authorized_keys` must not be able to write there, and a
/// component that only becomes a traversal after sanitising must not either.
fn sanitize_component(raw: &str) -> String {
    let cleaned = raw.replace(['\\', '/'], "_");
    let p = std::path::Path::new(&cleaned);
    let safe = matches!(p.components().next(), Some(Component::Normal(_)));
    if !safe || cleaned == "." || cleaned == ".." {
        return "_".into();
    }
    let s = sanitize_filename(&cleaned);
    if s.is_empty() {
        "_".into()
    } else {
        s
    }
}

fn hash_info(info_bytes: &[u8]) -> [u8; 20] {
    let mut h = Sha1::new();
    h.update(info_bytes);
    h.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Minimal single-file torrent built by hand.
    fn sample() -> Vec<u8> {
        let ann = b"http://tracker.example/announce";
        let mut info = Vec::new();
        info.extend_from_slice(b"d6:lengthi7e4:name4:test12:piece lengthi4e6:pieces40:");
        info.extend_from_slice(&[b'a'; 20]);
        info.extend_from_slice(&[b'b'; 20]);
        info.push(b'e');
        let mut r = Vec::new();
        r.extend_from_slice(b"d8:announce");
        r.extend_from_slice(ann.len().to_string().as_bytes());
        r.push(b':');
        r.extend_from_slice(ann);
        r.extend_from_slice(b"4:info");
        r.extend_from_slice(&info);
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
        assert_eq!(t.num_pieces(), 2);
        assert_eq!(t.piece_size(0), 4);
        assert_eq!(t.piece_size(1), 3);
        assert_eq!(t.announce, "http://tracker.example/announce");
        assert_eq!(t.files.len(), 1);
        assert_eq!(t.files[0].offset, 0);
        assert_eq!(t.files[0].length, 7);
    }

    #[test]
    fn a_multi_file_torrent_becomes_one_address_space() {
        let mut info = Vec::new();
        info.extend_from_slice(b"d5:filesl");
        info.extend_from_slice(b"d6:lengthi4e4:pathl1:a1:xee");
        info.extend_from_slice(b"d6:lengthi6e4:pathl1:b1:yee");
        info.extend_from_slice(b"e");
        info.extend_from_slice(b"4:name3:dir12:piece lengthi5e6:pieces40:");
        info.extend_from_slice(&[b'c'; 20]);
        info.extend_from_slice(&[b'd'; 20]);
        info.extend_from_slice(b"e");
        let mut r = Vec::new();
        r.extend_from_slice(b"d4:info");
        r.extend_from_slice(&info);
        r.push(b'e');

        let t = Torrent::from_bytes(&r).unwrap();
        assert_eq!(t.length, 10);
        assert_eq!(t.files.len(), 2);
        assert_eq!(t.files[0].display_name(), "dir/a/x");
        assert_eq!(t.files[1].offset, 4);
        assert_eq!(t.files[1].display_name(), "dir/b/y");
        // bytes 2..8 cross the file boundary
        let segs = t.segments(2, 6);
        assert_eq!(segs, vec![(0, 2, 2), (1, 0, 4)]);
        assert_eq!(t.file_of_piece(0), 0);
        assert_eq!(t.file_of_piece(1), 1); // piece 1 starts at byte 5
        assert_eq!(t.pieces_for_range(0, 10), 0..2);
        assert!(!t.private);
    }

    #[test]
    fn traversal_components_are_neutralised() {
        let mut info = Vec::new();
        info.extend_from_slice(b"d5:filesl");
        info.extend_from_slice(b"d6:lengthi4e4:pathl2:..4:pass4:1234eee");
        info.extend_from_slice(b"4:name4:root12:piece lengthi4e6:pieces20:");
        info.extend_from_slice(&[b'e'; 20]);
        info.push(b'e');
        let mut r = Vec::new();
        r.extend_from_slice(b"d4:info");
        r.extend_from_slice(&info);
        r.push(b'e');

        let t = Torrent::from_bytes(&r).unwrap();
        assert_eq!(t.files[0].path, vec!["root", "_", "pass", "1234"]);
    }

    #[test]
    fn a_truncated_piece_table_is_refused() {
        // length 7 needs two pieces of 4, but only one is listed.
        let ann = b"http://tracker.example/announce";
        let mut info = Vec::new();
        info.extend_from_slice(b"d6:lengthi7e4:name4:test12:piece lengthi4e6:pieces20:");
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
        assert!(format!("{err}").contains("pieces"), "{err}");
    }

    #[test]
    fn info_hash_stable() {
        let data = sample();
        let t1 = Torrent::from_bytes(&data).unwrap();
        let t2 = Torrent::from_bytes(&data).unwrap();
        assert_eq!(t1.info_hash, t2.info_hash);
        assert_eq!(t1.metainfo(), t1.metainfo());
        // Re-reading our own metainfo yields the same hash.
        let again = Torrent::from_bytes(&t1.metainfo()).unwrap();
        assert_eq!(again.info_hash, t1.info_hash);
    }

    #[test]
    fn a_non_canonical_info_dict_keeps_its_original_hash() {
        // Keys deliberately out of order. A re-encode would sort them (or at
        // least emit them in a different order) and hash a different torrent.
        let info = b"d6:pieces20:aaaaaaaaaaaaaaaaaaaa4:name4:test12:piece lengthi4e6:lengthi4ee";
        let mut root = Vec::new();
        root.extend_from_slice(b"d4:info");
        root.extend_from_slice(info);
        root.push(b'e');

        let t = Torrent::from_bytes(&root).unwrap();
        let mut h = Sha1::new();
        h.update(info);
        let expected: [u8; 20] = h.finalize().into();
        assert_eq!(t.info_hash, expected);
        assert_eq!(t.info_bytes, info);

        // The same bytes arriving from a peer must be accepted by the magnet
        // path, which is where a re-encode used to change the hash.
        let from_peer = Torrent::from_metadata(info, expected).unwrap();
        assert_eq!(from_peer.info_hash, expected);
    }
}
