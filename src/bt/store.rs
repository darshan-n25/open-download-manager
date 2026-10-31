//! The torrent's content on disk.
//!
//! A torrent is one address space of `length` bytes, but it may be many files. The
//! store hides that: [`Store::write_at`] and [`Store::read_at`] take a torrent offset
//! and split it across the files it touches, so the piece picker never has to know
//! where a file boundary is.
//!
//! Every write is positional (`pwrite`/`seek_write`), which means the peer connections
//! verifying pieces, the streaming server reading a range, and the resume check can all
//! touch the same file at once without a lock.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::bt::torrent::Torrent;
use crate::error::{OdmError, Result};
use crate::platform::{read_all_at, write_all_at};

struct FilePart {
    file: Arc<std::fs::File>,
    /// Where this file starts in the torrent's address space.
    start: u64,
    len: u64,
    path: PathBuf,
}

pub struct Store {
    parts: Vec<FilePart>,
    total: u64,
    base: PathBuf,
    /// True when at least one file had to be created or extended. Whatever those
    /// files hold was never written by this torrent, so a resume bitfield cannot
    /// describe them: a deleted download comes back as a full-length file of zeros
    /// once it is preallocated, and trusting an old bitfield would call that finished.
    fresh: bool,
}

impl Store {
    /// Create (or reopen) every file of the torrent under `base`.
    ///
    /// Files are extended to their final length up front. On Unix that is a sparse
    /// allocation; on Windows the clusters are reserved. Either way a download that
    /// cannot fit fails at the start instead of at 99%, and resume works because an
    /// existing file is never truncated.
    pub fn create(base: &Path, torrent: &Torrent) -> Result<Store> {
        let mut parts = Vec::with_capacity(torrent.files.len());
        let mut fresh = false;
        for entry in &torrent.files {
            if entry.length == 0 {
                continue;
            }
            let rel = entry.relative_path();
            let path = base.join(&rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| OdmError::other(format!("{}: {e}", parent.display())))?;
            }
            let file = std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(&path)
                .map_err(|e| OdmError::other(format!("{}: {e}", path.display())))?;
            let have = file.metadata().map(|m| m.len()).unwrap_or(0);
            if have < entry.length {
                file.set_len(entry.length)
                    .map_err(|e| OdmError::other(format!("{}: {e}", path.display())))?;
                fresh = true;
            }
            parts.push(FilePart {
                file: Arc::new(file),
                start: entry.offset,
                len: entry.length,
                path,
            });
        }
        Ok(Store {
            parts,
            total: torrent.length,
            base: base.to_path_buf(),
            fresh,
        })
    }

    /// Whether any file had to be created or extended when the store was opened.
    ///
    /// A resume bitfield only describes bytes that were already on disk, so it must
    /// be ignored when this is true.
    pub fn is_fresh(&self) -> bool {
        self.fresh
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn base(&self) -> &Path {
        &self.base
    }

    pub fn paths(&self) -> Vec<PathBuf> {
        self.parts.iter().map(|p| p.path.clone()).collect()
    }

    /// Write `buf` at a torrent offset, across as many files as the range touches.
    pub fn write_at(&self, offset: u64, buf: &[u8]) -> std::io::Result<()> {
        let end = offset + buf.len() as u64;
        if end > self.total {
            return Err(std::io::Error::other(format!(
                "write of {} bytes at {offset} runs past the torrent's {total} bytes",
                buf.len(),
                total = self.total
            )));
        }
        for part in &self.parts {
            let pstart = part.start;
            let pend = part.start + part.len;
            if pend <= offset || pstart >= end {
                continue;
            }
            let from = offset.max(pstart);
            let to = end.min(pend);
            let slice = &buf[(from - offset) as usize..(to - offset) as usize];
            write_all_at(&part.file, slice, from - pstart)?;
        }
        Ok(())
    }

    /// Read `buf.len()` bytes at a torrent offset, across files if needed.
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        let end = offset + buf.len() as u64;
        if end > self.total {
            return Err(std::io::Error::other(format!(
                "read of {} bytes at {offset} runs past the torrent's {total} bytes",
                buf.len(),
                total = self.total
            )));
        }
        for part in &self.parts {
            let pstart = part.start;
            let pend = part.start + part.len;
            if pend <= offset || pstart >= end {
                continue;
            }
            let from = offset.max(pstart);
            let to = end.min(pend);
            let slice = &mut buf[(from - offset) as usize..(to - offset) as usize];
            read_all_at(&part.file, slice, from - pstart)?;
        }
        Ok(())
    }

    pub fn sync(&self) -> std::io::Result<()> {
        for part in &self.parts {
            part.file.sync_all()?;
        }
        Ok(())
    }

    /// Delete whatever this torrent has written so far.
    pub fn remove(&self) {
        for part in &self.parts {
            let _ = std::fs::remove_file(&part.path);
        }
        // Directories the torrent created are left alone when they still hold
        // something else (a user's own file, another torrent's output).
        for part in &self.parts {
            let mut dir = part.path.parent();
            while let Some(d) = dir {
                if d == self.base || !d.starts_with(&self.base) {
                    break;
                }
                if std::fs::remove_dir(d).is_err() {
                    break;
                }
                dir = d.parent();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bt::torrent::Torrent;

    fn multi_file_torrent() -> Torrent {
        let mut info = Vec::new();
        info.extend_from_slice(b"d5:filesl");
        info.extend_from_slice(b"d6:lengthi4e4:pathl1:a1:xee");
        info.extend_from_slice(b"d6:lengthi6e4:pathl1:b1:yee");
        info.extend_from_slice(b"e");
        info.extend_from_slice(b"4:name3:dir12:piece lengthi5e6:pieces40:");
        info.extend_from_slice(&[b'c'; 40]);
        info.extend_from_slice(b"e");
        let mut r = Vec::new();
        r.extend_from_slice(b"d4:info");
        r.extend_from_slice(&info);
        r.push(b'e');
        Torrent::from_bytes(&r).unwrap()
    }

    #[test]
    fn a_write_can_cross_a_file_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let t = multi_file_torrent();
        let store = Store::create(dir.path(), &t).unwrap();
        // 8 bytes starting at byte 2: 2 into the first file, 6 into the second.
        store.write_at(2, b"01234567").unwrap();
        let a = std::fs::read(dir.path().join("dir/a/x")).unwrap();
        let b = std::fs::read(dir.path().join("dir/b/y")).unwrap();
        assert_eq!(a, vec![0, 0, b'0', b'1']);
        assert_eq!(b, b"234567".to_vec());
    }

    #[test]
    fn a_read_can_cross_a_file_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let t = multi_file_torrent();
        let store = Store::create(dir.path(), &t).unwrap();
        std::fs::write(dir.path().join("dir/a/x"), b"AAAA").unwrap();
        std::fs::write(dir.path().join("dir/b/y"), b"BBBBBB").unwrap();
        let mut buf = [0u8; 8];
        store.read_at(2, &mut buf).unwrap();
        assert_eq!(&buf, b"AABBBBBB");
    }

    #[test]
    fn resume_reopens_without_truncating() {
        let dir = tempfile::tempdir().unwrap();
        let t = multi_file_torrent();
        {
            let store = Store::create(dir.path(), &t).unwrap();
            store.write_at(0, b"KEEP").unwrap();
        }
        let store = Store::create(dir.path(), &t).unwrap();
        let mut buf = [0u8; 4];
        store.read_at(0, &mut buf).unwrap();
        assert_eq!(&buf, b"KEEP");
    }

    #[test]
    fn a_created_file_makes_the_store_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let t = multi_file_torrent();
        // Nothing is on disk yet, so no resume bitfield can describe these files —
        // they only look complete because `create` extends them to their final size.
        assert!(Store::create(dir.path(), &t).unwrap().is_fresh());
        // Second time the files are already at their final length: that is a resume.
        assert!(!Store::create(dir.path(), &t).unwrap().is_fresh());
    }
}
