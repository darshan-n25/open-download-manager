use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::segment::DEFAULT_MIN_CHUNK;

/// Default browser-ish user agent. Servers commonly gate content on this, which is why
/// IDM also forwards the browser's own `User-Agent` when the extension captures a link.
pub const DEFAULT_UA: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/124.0.0.0 Safari/537.36 Riptide/0.1";

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    /// Desired parallel connections per job ("Max. connections number" in IDM).
    pub connections: usize,
    /// Smallest useful segment; also the floor for runtime re-splitting.
    pub min_chunk: u64,
    /// "Number of retries for each file if downloading failed".
    pub max_retries: u32,
    /// "Max. connections number for a server": simultaneous connections to any one host,
    /// shared across every job. 0 disables the ceiling.
    pub per_server_connections: usize,
    /// Global ceiling across all jobs, bytes/second ("Maximum download speed").
    pub speed_limit: Option<u64>,
    /// IDM's "Download limits": total bytes allowed per window, across all jobs.
    /// `None` or `Some(0)` is unlimited.
    pub quota_bytes: Option<u64>,
    /// Length of the quota window, in seconds. Default is one hour.
    pub quota_window_secs: u64,
    pub proxy: Option<String>,
    pub user_agent: String,
    pub download_dir: PathBuf,
    pub state_dir: PathBuf,
    /// How many jobs may transfer at once (queue width).
    pub max_concurrent_jobs: usize,
    pub insecure: bool,
    /// Auto-sort finished files into per-type subfolders, as IDM's categories do.
    pub auto_categorize: bool,
    pub daemon_addr: String,
    /// File holding the bearer token every daemon request must present. `None` means the
    /// daemon is unauthenticated, which is only appropriate while it is bound to loopback.
    ///
    /// A *path*, not the secret: the config file is often world-readable, and a path can be
    /// shared with a service unit without copying the token into two places.
    ///
    /// Serialised as an explicit `null` rather than skipped, so `config set daemon_token_file`
    /// can find the key (and `config show` shows it, like every other optional setting).
    #[serde(default)]
    pub daemon_token_file: Option<PathBuf>,
    pub connect_timeout_secs: u64,
    /// Idle read timeout — IDM's "Connection restarted because of timeout".
    pub read_timeout_secs: u64,
    pub retry_backoff_ms: u64,
    /// Only bother re-splitting when the remainder exceeds this many bytes.
    pub split_threshold: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            connections: 8,
            min_chunk: DEFAULT_MIN_CHUNK,
            max_retries: 5,
            // Roomy enough that a single job is never clipped, tight enough that several
            // jobs hitting one CDN do not look like an attack.
            per_server_connections: 16,
            speed_limit: None,
            quota_bytes: None,
            quota_window_secs: 3600,
            proxy: None,
            user_agent: DEFAULT_UA.to_string(),
            download_dir: home_dir().join("Downloads"),
            state_dir: data_dir(),
            max_concurrent_jobs: 3,
            insecure: false,
            auto_categorize: true,
            daemon_addr: "127.0.0.1:1001".to_string(),
            daemon_token_file: None,
            connect_timeout_secs: 15,
            read_timeout_secs: 30,
            retry_backoff_ms: 500,
            split_threshold: 4 << 20,
        }
    }
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn data_dir() -> PathBuf {
    if let Some(x) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(x).join("riptide");
    }
    home_dir().join(".local/share/riptide")
}

fn config_dir() -> PathBuf {
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(x).join("riptide");
    }
    home_dir().join(".config/riptide")
}

impl Config {
    pub fn path() -> PathBuf {
        // `RIPTIDE_CONFIG` wins, for tests and for running several profiles.
        if let Some(p) = std::env::var_os("RIPTIDE_CONFIG") {
            return PathBuf::from(p);
        }
        config_dir().join("config.json")
    }

    pub fn load() -> Self {
        let path = Self::path();
        match std::fs::read(&path) {
            Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let data = serde_json::to_vec_pretty(self).unwrap_or_default();
        crate::util::atomic_write(&Self::path(), &data)
    }

    /// Build the process-wide budget from this configuration.
    pub fn quota(&self) -> crate::quota::Quota {
        crate::quota::Quota::new(
            self.quota_bytes,
            std::time::Duration::from_secs(self.quota_window_secs),
        )
    }

    /// Like [`Config::quota`], but remembered under `state_dir` so that restarting the
    /// process resumes the window instead of handing back a full budget.
    ///
    /// The queue owner and every other engine use this, so crawl traffic counts against
    /// the same budget as downloads. With no limit configured it reads nothing and writes
    /// nothing.
    pub fn persistent_quota(&self) -> crate::quota::Quota {
        crate::quota::Quota::persistent(
            self.quota_bytes,
            std::time::Duration::from_secs(self.quota_window_secs),
            self.state_dir.join("quota.json"),
        )
    }

    /// Resolve the destination for a finished download, applying IDM-style category
    /// sorting when enabled.
    pub fn resolve_output(
        &self,
        explicit: Option<PathBuf>,
        filename: &str,
        ext: Option<&str>,
    ) -> PathBuf {
        if let Some(p) = explicit {
            return p;
        }
        let mut dir = self.download_dir.clone();
        if self.auto_categorize {
            if let Some(ext) = ext {
                let cat = crate::util::category_for_extension(ext);
                if cat != "General" {
                    dir = dir.join(cat);
                }
            }
        }
        let candidate = dir.join(crate::util::sanitize_filename(filename));
        if candidate.exists() {
            // Never silently clobber: add a numeric suffix like every browser does.
            let stem = candidate
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("download")
                .to_string();
            let ext = candidate
                .extension()
                .and_then(|s| s.to_str())
                .map(|e| format!(".{e}"))
                .unwrap_or_default();
            for n in 1..10_000 {
                let alt = dir.join(format!("{stem} ({n}){ext}"));
                if !alt.exists() {
                    return alt;
                }
            }
        }
        candidate
    }
}

#[cfg(test)]
mod tests {
    // Fixtures here are built field-by-field from `Default::default()`.
    #![allow(clippy::field_reassign_with_default)]
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.connections, 8);
        assert!(c.min_chunk >= 4096);
        assert!(c.max_concurrent_jobs >= 1);
    }

    #[test]
    fn explicit_output_wins() {
        let c = Config::default();
        let p = PathBuf::from("/tmp/explicit.bin");
        assert_eq!(c.resolve_output(Some(p.clone()), "x", Some("mp4")), p);
    }

    #[test]
    fn categorisation_routes_by_extension() {
        let mut c = Config::default();
        c.download_dir = PathBuf::from("/tmp/dl");
        c.auto_categorize = true;
        assert_eq!(
            c.resolve_output(None, "movie.mp4", Some("mp4")),
            PathBuf::from("/tmp/dl/Video/movie.mp4")
        );
        assert_eq!(
            c.resolve_output(None, "notes.txt", Some("txt")),
            PathBuf::from("/tmp/dl/Documents/notes.txt")
        );
        c.auto_categorize = false;
        assert_eq!(
            c.resolve_output(None, "movie.mp4", Some("mp4")),
            PathBuf::from("/tmp/dl/movie.mp4")
        );
    }

    #[test]
    fn config_round_trips_through_json() {
        let mut c = Config::default();
        c.connections = 16;
        c.speed_limit = Some(1234);
        let s = serde_json::to_string(&c).unwrap();
        let back: Config = serde_json::from_str(&s).unwrap();
        assert_eq!(back.connections, 16);
        assert_eq!(back.speed_limit, Some(1234));
    }

    #[test]
    fn the_quota_comes_from_the_config() {
        let mut c = Config::default();
        assert!(c.quota().is_unlimited());

        c.quota_bytes = Some(500 << 20);
        c.quota_window_secs = 7200;
        let q = c.quota();
        assert_eq!(q.limit_bytes(), Some(500 << 20));
        assert_eq!(q.window(), std::time::Duration::from_secs(7200));
    }

    #[test]
    fn the_persistent_quota_lives_in_the_state_directory() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Config::default();
        c.state_dir = dir.path().to_path_buf();
        c.quota_bytes = Some(1000);
        c.quota_window_secs = 3600;

        let q = c.persistent_quota();
        q.charge(250).unwrap();
        q.flush();

        assert!(
            dir.path().join("quota.json").exists(),
            "the window should have been remembered in the state directory"
        );
        let again = c.persistent_quota();
        assert_eq!(again.used(), 250);
    }

    #[test]
    fn a_config_without_quota_keys_still_loads() {
        // `#[serde(default)]` with a newer field: an old config file must not be rejected.
        let json = r#"{"connections": 4, "speed_limit": null}"#;
        let c: Config = serde_json::from_str(json).unwrap();
        assert_eq!(c.connections, 4);
        assert_eq!(c.quota_bytes, None);
        assert_eq!(c.quota_window_secs, 3600);
    }
}
