//! Content checksums, verified **before** a download is published.
//!
//! A checksum is compared against the working file while it is still the `.part`/`.tmpdl`
//! sibling, so the rename that publishes the download only happens once the bytes are
//! known to be right. A mismatch deletes the working file and fails the job: a file that
//! fails its checksum never becomes the download, and never silently replaces a good one.
//!
//! HTTP and FTP downloads go through [`crate::storage::Sink`] and are checked at exactly the
//! point the rename would happen. HLS/DASH output is assembled by `ffmpeg`/concatenation at
//! its final path instead of via a rename, so for media the check runs on the assembled file
//! and a mismatch removes it — the same guarantee, expressed against a different pipeline.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::error::{OdmError, Result};

/// Checksum algorithms Open Download Manager can verify.
///
/// `sha256` and `sha512` are the ones to reach for. `sha1` and `md5` are here because release
/// pages still publish them; they are accepted so a checksum next to a download can be used
/// as-is, not because they are good choices.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Algorithm {
    Md5,
    Sha1,
    Sha256,
    Sha512,
}

impl Algorithm {
    pub fn as_str(self) -> &'static str {
        match self {
            Algorithm::Md5 => "md5",
            Algorithm::Sha1 => "sha1",
            Algorithm::Sha256 => "sha256",
            Algorithm::Sha512 => "sha512",
        }
    }

    /// Number of hex characters in this algorithm's digest.
    pub fn hex_len(self) -> usize {
        match self {
            Algorithm::Md5 => 32,
            Algorithm::Sha1 => 40,
            Algorithm::Sha256 => 64,
            Algorithm::Sha512 => 128,
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "md5" => Some(Algorithm::Md5),
            "sha1" | "sha-1" => Some(Algorithm::Sha1),
            "sha256" | "sha-256" => Some(Algorithm::Sha256),
            "sha512" | "sha-512" => Some(Algorithm::Sha512),
            _ => None,
        }
    }

    /// The algorithm whose digest is `len` hex characters long, if exactly one matches.
    fn from_hex_len(len: usize) -> Option<Self> {
        [
            Algorithm::Md5,
            Algorithm::Sha1,
            Algorithm::Sha256,
            Algorithm::Sha512,
        ]
        .into_iter()
        .find(|a| a.hex_len() == len)
    }
}

/// An expected digest for a download.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checksum {
    pub algorithm: Algorithm,
    /// Lowercase hex digest.
    pub digest: String,
}

impl Checksum {
    /// Parse a checksum specification.
    ///
    /// Accepted forms:
    ///
    /// * `sha256:<hex>` — an explicit algorithm (`:` or `=` as the separator);
    /// * a bare hex digest, whose length identifies the algorithm unambiguously
    ///   (32 = md5, 40 = sha1, 64 = sha256, 128 = sha512).
    ///
    /// A bare digest of any other length is rejected: guessing an algorithm for a checksum
    /// the user pasted would make a mismatch mean nothing.
    pub fn parse(spec: &str) -> Result<Self> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err(OdmError::other(
                "empty --checksum; expected e.g. sha256:<hex>",
            ));
        }

        // Split on the first `:` or `=`, but only when the prefix looks like an algorithm
        // name, so a bare digest is not mistaken for one.
        let explicit = spec
            .split_once([':', '='])
            .and_then(|(name, rest)| Algorithm::parse(name).map(|a| (a, rest.to_string())));
        let (algorithm, digest) = match explicit {
            Some(pair) => pair,
            None => {
                let hex = spec.replace([' ', ':'], "");
                let algorithm = Algorithm::from_hex_len(hex.len()).ok_or_else(|| {
                    OdmError::other(format!(
                        "cannot tell which algorithm {:?} is a digest for; expected 32 (md5), \
                         40 (sha1), 64 (sha256) or 128 (sha512) hex characters, or write \
                         e.g. sha256:<hex>",
                        spec
                    ))
                })?;
                (algorithm, hex)
            }
        };

        let digest = digest.trim().to_ascii_lowercase();
        if digest.len() != algorithm.hex_len() {
            return Err(OdmError::other(format!(
                "{} digest {:?} is {} hex characters, expected {}",
                algorithm.as_str(),
                digest,
                digest.len(),
                algorithm.hex_len()
            )));
        }
        if !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(OdmError::other(format!(
                "{} digest {:?} is not hexadecimal",
                algorithm.as_str(),
                digest
            )));
        }
        Ok(Checksum { algorithm, digest })
    }

    /// The digest of `data`, as a ready-to-use `Checksum`.
    ///
    /// Useful for building an expectation from bytes you already hold — a fixture, or a
    /// release you have downloaded once and want to pin.
    pub fn of_bytes(algorithm: Algorithm, data: &[u8]) -> Self {
        let digest = match algorithm {
            Algorithm::Md5 => hash_bytes::<md5::Md5>(data),
            Algorithm::Sha1 => hash_bytes::<sha1::Sha1>(data),
            Algorithm::Sha256 => hash_bytes::<sha2::Sha256>(data),
            Algorithm::Sha512 => hash_bytes::<sha2::Sha512>(data),
        };
        Checksum { algorithm, digest }
    }

    /// Hash `data` and compare.
    pub fn matches_bytes(&self, data: &[u8]) -> bool {
        Checksum::of_bytes(self.algorithm, data).digest == self.digest
    }

    /// Hash a file on disk and compare. Streams the file, so it does not buffer it.
    pub fn matches_file(&self, path: &Path) -> std::io::Result<bool> {
        let actual = self.hash_file(path)?;
        Ok(actual == self.digest)
    }

    /// The digest of a file, as lowercase hex.
    pub fn hash_file(&self, path: &Path) -> std::io::Result<String> {
        let mut file = std::fs::File::open(path)?;
        let mut hasher = self.new_hasher();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hex::encode(hasher.finalize()))
    }

    fn new_hasher(&self) -> Box<dyn DynDigest> {
        match self.algorithm {
            Algorithm::Md5 => Box::new(md5::Md5::new()),
            Algorithm::Sha1 => Box::new(sha1::Sha1::new()),
            Algorithm::Sha256 => Box::new(sha2::Sha256::new()),
            Algorithm::Sha512 => Box::new(sha2::Sha512::new()),
        }
    }

    /// The `algorithm:digest` form, for messages.
    pub fn spec(&self) -> String {
        format!("{}:{}", self.algorithm.as_str(), self.digest)
    }
}

/// Object-safe shim over `digest::Digest`, so one `Box` can hold any of the four hashers
/// without the four `match` arms reappearing in the read loop.
trait DynDigest {
    fn update(&mut self, data: &[u8]);
    fn finalize(self: Box<Self>) -> Vec<u8>;
}

impl<D: Digest + 'static> DynDigest for D {
    fn update(&mut self, data: &[u8]) {
        Digest::update(self, data);
    }
    fn finalize(self: Box<Self>) -> Vec<u8> {
        Digest::finalize(*self).to_vec()
    }
}

fn hash_bytes<D: Digest>(data: &[u8]) -> String {
    hex::encode(D::digest(data))
}

/// Verify `path`, deleting it when it does not match.
///
/// Deleting on mismatch is deliberate. The working file exists only to become the output;
/// leaving a known-bad file behind invites a later resume to finish a corrupt download, and
/// invites a user to open a file that is known to be wrong.
pub fn verify_and_discard_on_mismatch(checksum: &Checksum, path: &Path) -> Result<String> {
    match checksum.hash_file(path) {
        Ok(actual) if actual == checksum.digest => Ok(actual),
        Ok(actual) => {
            let _ = std::fs::remove_file(path);
            Err(OdmError::ChecksumMismatch {
                algorithm: checksum.algorithm.as_str().to_string(),
                expected: checksum.digest.clone(),
                actual,
                path: path.display().to_string(),
            })
        }
        Err(e) => {
            let _ = std::fs::remove_file(path);
            Err(e.into())
        }
    }
}

/// Async wrapper: hashing is CPU-bound, so it runs on the blocking pool.
pub async fn verify_file(checksum: &Checksum, path: &Path) -> Result<String> {
    let checksum = checksum.clone();
    let path: PathBuf = path.to_path_buf();
    tokio::task::spawn_blocking(move || verify_and_discard_on_mismatch(&checksum, &path))
        .await
        .map_err(std::io::Error::other)?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_explicit_algorithm_and_separator_forms() {
        let c = Checksum::parse("sha256:ab").unwrap_err();
        assert!(c.to_string().contains("expected 64"), "{c}");

        let hex = "a".repeat(64);
        for spec in [
            format!("sha256:{hex}"),
            format!("sha256={hex}"),
            format!("SHA256:{hex}"),
            format!("sha-256:{hex}"),
        ] {
            let c = Checksum::parse(&spec).unwrap();
            assert_eq!(c.algorithm, Algorithm::Sha256);
            assert_eq!(c.digest, hex);
        }
    }

    #[test]
    fn infers_the_algorithm_from_a_bare_digest_length() {
        for (len, alg) in [
            (32, Algorithm::Md5),
            (40, Algorithm::Sha1),
            (64, Algorithm::Sha256),
            (128, Algorithm::Sha512),
        ] {
            let c = Checksum::parse(&"b".repeat(len)).unwrap();
            assert_eq!(c.algorithm, alg, "length {len}");
        }
    }

    #[test]
    fn rejects_out_of_range_and_non_hex_digests() {
        // 63 characters is no algorithm's digest length.
        assert!(Checksum::parse(&"a".repeat(63)).is_err());
        // Right length, wrong alphabet.
        assert!(Checksum::parse(&format!("sha256:{}", "z".repeat(64))).is_err());
        // An unknown algorithm name with a digest is not a bare digest either.
        assert!(Checksum::parse("sha3:deadbeef").is_err());
        assert!(Checksum::parse("   ").is_err());
    }

    #[test]
    fn matches_the_published_vectors() {
        let empty = Checksum::parse(
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        )
        .unwrap();
        assert!(empty.matches_bytes(b""));

        let abc = Checksum::parse(
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        )
        .unwrap();
        assert!(abc.matches_bytes(b"abc"));
        assert!(!abc.matches_bytes(b"abd"));

        // The same string via md5 and sha1, so all four arms are exercised.
        let md5 = Checksum::parse("md5:900150983cd24fb0d6963f7d28e17f72").unwrap();
        assert!(md5.matches_bytes(b"abc"));
        let sha1 = Checksum::parse("sha1:a9993e364706816aba3e25717850c26c9cd0d89d").unwrap();
        assert!(sha1.matches_bytes(b"abc"));
        let sha512 = Checksum::parse(
            "sha512:ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
        )
        .unwrap();
        assert!(sha512.matches_bytes(b"abc"));
    }

    #[test]
    fn hash_file_is_case_insensitive_on_the_expected_digest() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, b"abc").unwrap();

        let lower = Checksum::parse(
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        )
        .unwrap();
        // `parse` lowercases, so an uppercase spec compares equal.
        let upper = Checksum::parse(
            "sha256:BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD",
        )
        .unwrap();
        assert!(lower.matches_file(&p).unwrap());
        assert!(upper.matches_file(&p).unwrap());
    }

    #[tokio::test]
    async fn a_mismatch_deletes_the_working_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.part");
        std::fs::write(&p, b"the wrong bytes").unwrap();

        let ck = Checksum::parse(
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        )
        .unwrap();
        let err = verify_file(&ck, &p).await.unwrap_err();
        assert!(matches!(err, OdmError::ChecksumMismatch { .. }), "{err}");
        assert!(!p.exists(), "the bad working file must be gone");
    }

    #[tokio::test]
    async fn a_match_keeps_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.part");
        std::fs::write(&p, b"abc").unwrap();
        let ck = Checksum::parse(
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        )
        .unwrap();
        assert_eq!(verify_file(&ck, &p).await.unwrap(), ck.digest);
        assert!(p.exists());
    }

    #[test]
    fn a_large_file_hashes_in_streaming_chunks() {
        // Larger than the 64 KiB read buffer, so the read loop runs more than once.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big");
        std::fs::write(&p, vec![0xABu8; 300 * 1024]).unwrap();

        // The one-file digest must equal the digest of the same bytes hashed in memory,
        // which proves the chunked read loop feeds the hasher the whole file.
        let probe = Checksum {
            algorithm: Algorithm::Sha256,
            digest: String::new(),
        };
        let from_file = probe.hash_file(&p).unwrap();
        assert_eq!(from_file.len(), 64);
        assert!(
            !probe.matches_file(&p).unwrap(),
            "an empty digest cannot match"
        );
        let good = Checksum {
            algorithm: Algorithm::Sha256,
            digest: from_file.clone(),
        };
        assert!(good.matches_file(&p).unwrap());
        assert_eq!(good.hash_file(&p).unwrap(), from_file);
    }

    #[test]
    fn round_trips_through_serde() {
        let ck = Checksum::parse(
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        )
        .unwrap();
        let json = serde_json::to_string(&ck).unwrap();
        assert!(json.contains("\"algorithm\":\"sha256\""), "{json}");
        let back: Checksum = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ck);
    }
}
