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
    /// Global ceiling across all jobs, bytes/second ("Maximum download speed").
    pub speed_limit: Option<u64>,
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
            speed_limit: None,
            proxy: None,
            user_agent: DEFAULT_UA.to_string(),
            download_dir: home_dir().join("Downloads"),
            state_dir: data_dir(),
            max_concurrent_jobs: 3,
            insecure: false,
            auto_categorize: true,
            daemon_addr: "127.0.0.1:1001".to_string(),
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
}
