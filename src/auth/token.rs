//! The daemon's own bearer token — the *server* side of authentication.
//!
//! The rest of [`crate::auth`] is about authenticating **to** other servers (Basic, Digest,
//! NTLM, Negotiate). This module is the reverse: the credential Riptide's daemon demands
//! from its own clients, so that listening somewhere other than loopback is a decision
//! rather than an accident.
//!
//! Three properties are deliberate.
//!
//! * **Constant-time comparison.** Both sides are hashed with SHA-256 first, so the compare
//!   is over a fixed 32 bytes whatever the input length, and a correct *prefix* is not
//!   observable in timing. A byte-wise `==` against a secret is a small but real oracle.
//! * **The token is never a command-line argument.** It comes from a file or the
//!   environment. On most systems `argv` is world-readable (`ps auxww` shows it to every
//!   user), while a `0600` file is not.
//! * **A configured token that cannot be read is a startup failure.** Falling back to "no
//!   token" on a typo would silently turn authentication off, which is the one failure mode
//!   that must never be quiet.

use std::path::Path;

use sha2::{Digest, Sha256};

use crate::error::{Result, RiptideError};

/// Environment variable holding the token literally — for containers and service managers,
/// which can inject a secret without writing a file.
pub const ENV_TOKEN: &str = "RIPTIDE_DAEMON_TOKEN";

/// A required daemon token, kept as its SHA-256 so a comparison is over a fixed width.
#[derive(Clone)]
pub struct Auth {
    digest: [u8; 32],
}

impl Auth {
    /// Build from a token. Surrounding whitespace is ignored, so `echo tok > file` works.
    pub fn new(token: &str) -> Self {
        Auth {
            digest: sha256(token.trim().as_bytes()),
        }
    }

    /// Read a token from a file.
    ///
    /// A missing or empty file is an error, not "no authentication": both are almost always
    /// a mistake, and the consequence of guessing wrong is an open daemon.
    pub fn from_file(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            RiptideError::other(format!(
                "cannot read the daemon token file {}: {e}",
                path.display()
            ))
        })?;
        if raw.trim().is_empty() {
            return Err(RiptideError::other(format!(
                "the daemon token file {} is empty; refusing to run with a token that \
                 authenticates nothing",
                path.display()
            )));
        }
        Ok(Auth::new(&raw))
    }

    /// Does `presented` match? The comparison inspects every byte of both digests.
    pub fn accepts(&self, presented: &str) -> bool {
        ct_eq(&self.digest, &sha256(presented.trim().as_bytes()))
    }

    /// Accept an `Authorization` header value of the form `Bearer <token>`.
    ///
    /// The scheme is matched case-insensitively, as RFC 7235 requires. Any other scheme —
    /// including a bare `Bearer` with no value — is refused rather than treated as a token.
    pub fn accepts_bearer(&self, header: &str) -> bool {
        match header.trim().split_once(' ') {
            Some((scheme, value)) if scheme.eq_ignore_ascii_case("bearer") => self.accepts(value),
            _ => false,
        }
    }
}

/// Resolve the daemon token from the first source that is configured.
///
/// Order: an explicit `--token-file`, then [`ENV_TOKEN`], then the `daemon_token_file`
/// setting. The first *configured* source wins, and one that cannot be read is an error
/// rather than a fall-through to the next — otherwise a typo in a path would quietly
/// disable authentication.
pub fn load(explicit_file: Option<&Path>, configured_file: Option<&Path>) -> Result<Option<Auth>> {
    load_from(
        explicit_file,
        std::env::var(ENV_TOKEN).ok().as_deref(),
        configured_file,
    )
}

/// [`load`] with the environment passed in, so tests do not have to mutate the process
/// environment (which `cargo test` shares across threads).
fn load_from(
    explicit_file: Option<&Path>,
    env_token: Option<&str>,
    configured_file: Option<&Path>,
) -> Result<Option<Auth>> {
    if let Some(path) = explicit_file {
        return Ok(Some(Auth::from_file(path)?));
    }
    if let Some(raw) = env_token {
        if raw.trim().is_empty() {
            return Err(RiptideError::other(format!(
                "{ENV_TOKEN} is set but empty; unset it or give it a value"
            )));
        }
        return Ok(Some(Auth::new(raw)));
    }
    match configured_file {
        Some(path) => Ok(Some(Auth::from_file(path)?)),
        None => Ok(None),
    }
}

/// Generate a fresh token, write it to `path` with owner-only permissions, and return it.
///
/// Refuses to overwrite: replacing a token while a daemon is using it would lock out every
/// client until the daemon is restarted with the new value, and that is not something a
/// command should do by surprise.
pub fn write_token_file(path: &Path) -> Result<String> {
    if path.exists() {
        return Err(RiptideError::other(format!(
            "{} already exists; delete it first if you really want to replace the token",
            path.display()
        )));
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let token = generate();
    write_private(path, token.as_bytes())?;
    Ok(token)
}

/// A 256-bit token as lowercase hex — 64 characters, no encoding needed anywhere.
fn generate() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

#[cfg(unix)]
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    // 0600 is applied at creation, not afterwards: a token that exists briefly as 0644 has
    // already been exposed.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(data)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    // Windows has no `mode` on `OpenOptions`, and inheriting a directory ACL is the right
    // answer there rather than guessing one here.
    let mut body = data.to_vec();
    body.push(b'\n');
    std::fs::write(path, body)?;
    Ok(())
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(bytes));
    out
}

/// Constant-time byte comparison: every byte is read, so there is no early exit whose timing
/// reveals how many leading bytes matched.
fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_with(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn only_the_exact_token_is_accepted() {
        let auth = Auth::new("s3cret-token");
        assert!(auth.accepts("s3cret-token"));
        // Surrounding whitespace comes from `echo` and from header parsing, not from the
        // token, so it is ignored on both sides.
        assert!(auth.accepts("  s3cret-token\n"));
        assert!(auth.accepts("s3cret-token "));

        assert!(!auth.accepts(""));
        assert!(!auth.accepts("s3cret-toke"));
        assert!(!auth.accepts("s3cret-tokenX"));
        assert!(!auth.accepts("S3cret-token"));
        assert!(!auth.accepts("s3cret_t0ken"));
    }

    #[test]
    fn a_token_can_contain_is_not_confused_by_hashing_a_different_width() {
        // Digests are hashed, so a very long guess and a one-byte guess are both compared as
        // 32 bytes; neither can be right.
        let auth = Auth::new("short");
        assert!(!auth.accepts(&"x".repeat(10_000)));
        assert!(!auth.accepts("s"));
    }

    #[test]
    fn the_bearer_scheme_is_matched_case_insensitively_and_nothing_else_is() {
        let auth = Auth::new("tok");
        assert!(auth.accepts_bearer("Bearer tok"));
        assert!(auth.accepts_bearer("bearer tok"));
        assert!(auth.accepts_bearer("BEARER   tok  "));
        assert!(!auth.accepts_bearer("Bearer"));
        assert!(!auth.accepts_bearer("Basic tok"));
        assert!(!auth.accepts_bearer("token"));
        assert!(!auth.accepts_bearer(""));
    }

    #[test]
    fn an_explicit_file_beats_the_environment_and_the_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let explicit = file_with(dir.path(), "explicit", "from-explicit\n");
        let configured = file_with(dir.path(), "configured", "from-config\n");

        let auth = load_from(Some(&explicit), Some("from-env"), Some(&configured))
            .unwrap()
            .unwrap();
        assert!(auth.accepts("from-explicit"));
        assert!(!auth.accepts("from-env"));
        assert!(!auth.accepts("from-config"));
    }

    #[test]
    fn the_environment_beats_the_config_file_and_the_config_file_is_the_last_resort() {
        let dir = tempfile::tempdir().unwrap();
        let configured = file_with(dir.path(), "configured", "from-config\n");

        let auth = load_from(None, Some("from-env"), Some(&configured))
            .unwrap()
            .unwrap();
        assert!(auth.accepts("from-env"));

        let auth = load_from(None, None, Some(&configured)).unwrap().unwrap();
        assert!(auth.accepts("from-config"));
    }

    #[test]
    fn nothing_configured_means_no_requirement() {
        assert!(load_from(None, None, None).unwrap().is_none());
    }

    #[test]
    fn a_configured_source_that_is_broken_is_an_error_not_an_open_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        let empty = file_with(dir.path(), "empty", "   \n");

        // A path typo must not silently disable authentication.
        assert!(load_from(Some(&missing), None, None).is_err());
        assert!(load_from(None, None, Some(&missing)).is_err());
        assert!(load_from(None, None, Some(&empty)).is_err());
        assert!(Auth::from_file(&empty).is_err());
        // And an empty env var is a mistake, not "no token".
        assert!(load_from(None, Some("  "), None).is_err());
    }

    #[test]
    fn a_generated_token_is_256_bits_of_hex_and_is_not_reused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        let first = write_token_file(&path).unwrap();

        assert_eq!(first.len(), 64);
        assert!(first.bytes().all(|b| b.is_ascii_hexdigit()));
        // The file holds exactly what was returned, and the newline is not part of it.
        assert!(Auth::from_file(&path).unwrap().accepts(&first));

        // Refuses to clobber: a running daemon would be locked out.
        assert!(write_token_file(&path).is_err());

        let second_path = dir.path().join("token2");
        let second = write_token_file(&second_path).unwrap();
        assert_ne!(first, second, "a fresh token must be fresh");
    }

    #[cfg(unix)]
    #[test]
    fn a_generated_token_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        write_token_file(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "got {:o}", mode & 0o777);
    }

    #[test]
    fn constant_time_compare_agrees_with_equality() {
        let a = sha256(b"one");
        let b = sha256(b"one");
        let c = sha256(b"two");
        assert!(ct_eq(&a, &b));
        assert!(!ct_eq(&a, &c));
        // Differing only in the last byte is still a mismatch.
        let mut d = a;
        d[31] ^= 1;
        assert!(!ct_eq(&a, &d));
    }
}
