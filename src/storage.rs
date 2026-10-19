//! Positional file writes.
//!
//! Riptide writes every segment **directly at its final offset** in a pre-allocated
//! sparse file. There is no "download N parts then concatenate" step — that extra
//! assembly pass is IDM's most-complained-about behaviour (see `docs/FEATURE-MAP.md`),
//! and skipping it means a finished download is finished instantly.
//!
//! Positional writes (`pwrite`) never touch the file cursor, so any number of workers
//! can write to the same file concurrently without locking.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;

use crate::platform::write_all_at;

pub struct Sink {
    path: PathBuf,
    file: Arc<std::fs::File>,
}

impl Sink {
    /// Open (creating if needed) `path` and, when the final size is known, extend it to
    /// that length up front.
    ///
    /// On Linux and macOS that is a sparse allocation, so no disk space is consumed until
    /// bytes are actually written. On Windows the length is set the same way but the
    /// clusters are allocated, so the full size is reserved up front; the trade is the same
    /// either way (a download that cannot fit fails immediately rather than at 99%), and the
    /// engine never depends on the gap being free — every segment writes into its own
    /// region.
    pub fn create(path: &Path, size: Option<u64>) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            // Deliberately *not* truncating: an existing part file may hold bytes from a
            // previous run, which is exactly what makes resume work.
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        if let Some(sz) = size {
            // Only grow; never truncate an existing partial download.
            if file.metadata()?.len() < sz {
                file.set_len(sz)?;
            }
        }
        Ok(Sink {
            path: path.to_path_buf(),
            file: Arc::new(file),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn len(&self) -> std::io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    pub fn is_empty(&self) -> std::io::Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Write `buf` at absolute `offset` without disturbing any other writer.
    pub async fn write_at(&self, buf: Bytes, offset: u64) -> std::io::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let file = self.file.clone();
        tokio::task::spawn_blocking(move || write_all_at(&file, &buf, offset))
            .await
            .map_err(std::io::Error::other)?
    }

    /// Shrink the file to exactly `len` (used when the server sent fewer bytes than the
    /// advertised `Content-Length`).
    pub fn truncate_to(&self, len: u64) -> std::io::Result<()> {
        self.file.set_len(len)
    }

    /// Flush file data to stable storage before we declare the download complete.
    pub async fn sync(&self) -> std::io::Result<()> {
        let file = self.file.clone();
        tokio::task::spawn_blocking(move || file.sync_all())
            .await
            .map_err(std::io::Error::other)?
    }

    /// Atomically move the completed part file onto its final name.
    ///
    /// The handle stays open across the rename, which is fine on Unix and fine on Windows
    /// only because Rust opens files with `FILE_SHARE_DELETE`; a `MoveFileEx` on a file that
    /// somebody else has open *without* that sharing mode (a virus scanner, say) fails with
    /// a sharing violation, so the error is reported rather than swallowed.
    pub fn finalize(&self, final_path: &Path) -> std::io::Result<()> {
        if let Some(parent) = final_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::rename(&self.path, final_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn concurrent_positional_writes_do_not_interfere() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.bin");
        let sink = Sink::create(&path, Some(1024)).unwrap();

        // Three writers filling three disjoint regions at once.
        let (a, b, c) = (sink.file.clone(), sink.file.clone(), sink.file.clone());
        let h1 = tokio::task::spawn_blocking(move || {
            for i in 0..128u64 {
                write_all_at(&a, &[0xAA; 1], i).unwrap();
            }
        });
        let h2 = tokio::task::spawn_blocking(move || {
            for i in 0..128u64 {
                write_all_at(&b, &[0xBB; 1], 512 + i).unwrap();
            }
        });
        let h3 = tokio::task::spawn_blocking(move || {
            for i in 0..128u64 {
                write_all_at(&c, &[0xCC; 1], 256 + i).unwrap();
            }
        });
        h1.await.unwrap();
        h2.await.unwrap();
        h3.await.unwrap();

        let data = std::fs::read(&path).unwrap();
        assert_eq!(data.len(), 1024);
        assert!(data[0..128].iter().all(|b| *b == 0xAA));
        assert!(data[256..384].iter().all(|b| *b == 0xCC));
        assert!(data[512..640].iter().all(|b| *b == 0xBB));
        // Untouched region stays sparse/zero.
        assert!(data[900..1024].iter().all(|b| *b == 0));
    }

    #[tokio::test]
    async fn write_at_async_path_and_finalize() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("x.part");
        let sink = Sink::create(&part, Some(16)).unwrap();
        sink.write_at(Bytes::from_static(b"hello"), 0)
            .await
            .unwrap();
        sink.write_at(Bytes::from_static(b"world"), 8)
            .await
            .unwrap();
        sink.sync().await.unwrap();
        assert_eq!(std::fs::read(&part).unwrap().len(), 16);

        let final_path = dir.path().join("x.bin");
        sink.finalize(&final_path).unwrap();
        assert!(!part.exists());
        let d = std::fs::read(&final_path).unwrap();
        assert_eq!(&d[0..5], b"hello");
        assert_eq!(&d[8..13], b"world");
    }

    #[test]
    fn create_does_not_truncate_existing_partial() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.bin");
        std::fs::write(&path, vec![7u8; 100]).unwrap();
        let sink = Sink::create(&path, Some(5)).unwrap();
        assert_eq!(
            sink.len().unwrap(),
            100,
            "must never shrink an existing part"
        );
    }
}
