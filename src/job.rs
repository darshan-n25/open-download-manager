use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::auth::AuthSpec;

/// What kind of transfer this job is.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum JobKind {
    Http,
    Hls,
    Dash,
    Ftp,
    Grabber,
}

impl JobKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            JobKind::Http => "http",
            JobKind::Hls => "hls",
            JobKind::Dash => "dash",
            JobKind::Ftp => "ftp",
            JobKind::Grabber => "grabber",
        }
    }

    /// Infer the transfer kind from a URL alone, before any network traffic.
    ///
    /// The CLI and the daemon both use this so that a `.m3u8` handed over by the browser
    /// extension is captured as HLS rather than fetched as an opaque byte stream.
    pub fn guess(url: &str) -> JobKind {
        let lower = url.to_ascii_lowercase();
        let path = lower.split(['?', '#']).next().unwrap_or(&lower);
        if lower.starts_with("ftp://") || lower.starts_with("ftps://") {
            JobKind::Ftp
        } else if path.contains(".m3u8") {
            JobKind::Hls
        } else if path.contains(".mpd") {
            JobKind::Dash
        } else {
            JobKind::Http
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    /// Waiting for a free slot in the queue.
    Queued,
    /// Waiting for a scheduled start time.
    Scheduled,
    /// Currently transferring.
    Active,
    /// Suspended by the user (resumable).
    Paused,
    Done,
    Failed,
}

impl JobState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, JobState::Done | JobState::Failed)
    }
}

/// Per-segment lifecycle. `done` counts bytes already committed to disk for this
/// segment, so a resumed segment becomes `Range: bytes=<start+done>-<end>`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SegmentState {
    Pending,
    Running,
    Done,
    Failed,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Segment {
    pub id: u64,
    /// Absolute first byte offset in the output file (inclusive).
    pub start: u64,
    /// Absolute last byte offset (inclusive). `u64::MAX` means "unknown end".
    pub end: u64,
    /// Bytes committed for this segment, counted from `start`.
    pub done: u64,
    pub state: SegmentState,
    pub attempts: u32,
}

impl Segment {
    pub fn new(id: u64, start: u64, end: u64) -> Self {
        Segment {
            id,
            start,
            end,
            done: 0,
            state: SegmentState::Pending,
            attempts: 0,
        }
    }

    /// Absolute offset the next byte for this segment must be written to.
    pub fn cursor(&self) -> u64 {
        self.start.saturating_add(self.done)
    }

    /// Bytes still outstanding (None when the end is unknown).
    pub fn remaining(&self) -> Option<u64> {
        if self.end == u64::MAX {
            None
        } else {
            Some(self.end.saturating_add(1).saturating_sub(self.cursor()))
        }
    }

    pub fn is_complete(&self) -> bool {
        self.state == SegmentState::Done
    }
}

/// HTTP request context captured for a job: everything needed to replay the request
/// the browser was about to make. Mirrors what IDM's extension forwards over `MSG#`.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct RequestContext {
    pub user_agent: Option<String>,
    pub referer: Option<String>,
    pub cookie: Option<String>,
    pub headers: Vec<(String, String)>,
    /// Proxy in either URL form (`socks5://127.0.0.1:9050`) or IDM's own form
    /// (`1:http://host:port;bypass1,bypass2`).
    pub proxy: Option<String>,
    pub auth: Option<AuthSpec>,
    /// Allow invalid TLS certificates (opt-in, mirrors IDM's "ignore certificate errors").
    pub insecure: bool,
}

/// A reference to one media segment inside an HLS or DASH manifest.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct MediaSegmentRef {
    pub url: String,
    /// Byte range within the resource, for `#EXT-X-BYTERANGE` / DASH `@mediaRange`.
    pub byte_range: Option<(u64, u64)>,
    pub seq: u64,
    pub duration: Option<f64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct MediaKey {
    pub method: String,
    pub uri: String,
    /// Explicit IV, if the manifest supplied one.
    pub iv: Option<Vec<u8>>,
    /// Resolved key bytes (fetched separately).
    pub key: Option<Vec<u8>>,
}

/// Resolved media plan: a flat, ordered list of resources to fetch and concatenate.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct MediaSpec {
    pub manifest_url: String,
    pub variant_url: Option<String>,
    pub variant_label: Option<String>,
    pub init_url: Option<String>,
    pub segments: Vec<MediaSegmentRef>,
    pub key: Option<MediaKey>,
    /// Separate audio track, when the manifest splits video and audio. Riptide muxes the
    /// two automatically instead of leaving the user with two stray files.
    #[serde(default)]
    pub audio: Option<Box<MediaSpec>>,
    /// `ts` (MPEG-TS, needs remux), `mp4` (fMP4, concatenable), `unknown`.
    pub container: String,
    pub total_bytes: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Job {
    pub id: String,
    pub url: String,
    /// URL after redirects (what the bytes actually came from).
    pub resolved_url: Option<String>,
    pub output: PathBuf,
    pub kind: JobKind,
    pub state: JobState,

    pub total: Option<u64>,
    /// `Accept-Ranges: bytes` seen AND total size known ⇒ segmented download possible.
    pub resumable: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,

    pub segments: Vec<Segment>,
    pub downloaded: u64,

    pub connections: usize,
    pub min_chunk: u64,
    pub max_retries: u32,
    /// Bytes/second ceiling for this job alone.
    pub speed_limit: Option<u64>,

    pub ctx: RequestContext,
    pub media: Option<MediaSpec>,

    pub scheduled_at: Option<u64>,
    /// Requested rendition for HLS/DASH: `best`, `worst`, or a height such as `720`.
    #[serde(default)]
    pub quality: Option<String>,
    /// True while `output` is still a guess (no `-o` was given), so `probe` may replace it
    /// once the real filename is known from `Content-Disposition`.
    #[serde(default)]
    pub output_provisional: bool,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub error: Option<String>,
    /// Set when the job stopped because the download quota ran out. It is `Paused`, not
    /// `Failed`, and it becomes runnable again by itself once the window refills — so
    /// callers can tell "waiting for budget" apart from "waiting for the user".
    #[serde(default)]
    pub quota_paused: bool,
    /// Expected digest of the finished file (`--checksum sha256:…`). Checked against the
    /// working file before it is renamed onto `output`, so a mismatch leaves the output
    /// untouched and removes the working file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<crate::checksum::Checksum>,
    /// The server answered a conditional request with `304 Not Modified`: the bytes
    /// already on disk are the current version, so there is nothing to fetch. Only a
    /// synchronization project sets this, by sending `If-None-Match` /
    /// `If-Modified-Since`.
    #[serde(default)]
    pub not_modified: bool,
}

impl Job {
    pub fn new(id: String, url: String, output: PathBuf, kind: JobKind) -> Self {
        let now = crate::util::now_ms();
        Job {
            id,
            url,
            resolved_url: None,
            output,
            kind,
            state: JobState::Queued,
            total: None,
            resumable: false,
            etag: None,
            last_modified: None,
            content_type: None,
            segments: Vec::new(),
            downloaded: 0,
            connections: 8,
            min_chunk: 1 << 20,
            max_retries: 5,
            speed_limit: None,
            ctx: RequestContext::default(),
            media: None,
            scheduled_at: None,
            quality: None,
            output_provisional: false,
            created_ms: now,
            updated_ms: now,
            error: None,
            quota_paused: false,
            checksum: None,
            not_modified: false,
        }
    }

    /// Percentage in `0.0..=100.0`, or `None` when the size is unknown.
    pub fn progress(&self) -> Option<f64> {
        let total = self.total?;
        if total == 0 {
            return Some(100.0);
        }
        Some((self.downloaded as f64 / total as f64 * 100.0).min(100.0))
    }

    pub fn is_finished(&self) -> bool {
        match self.total {
            Some(t) => self.downloaded >= t,
            None => self.state == JobState::Done,
        }
    }

    /// Working file. Bytes are written at their final offsets into this file, then it is
    /// renamed onto `output` — an instant metadata operation, never a data copy.
    pub fn part_path(&self) -> PathBuf {
        let mut s = self.output.clone().into_os_string();
        s.push(".part");
        PathBuf::from(s)
    }

    /// Display name: the output file's file name.
    pub fn display_name(&self) -> String {
        self.output
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("download")
            .to_string()
    }

    pub fn touch(&mut self) {
        self.updated_ms = crate::util::now_ms();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_transfer_kind_is_guessed_from_the_url() {
        assert_eq!(JobKind::guess("https://h/a/file.iso"), JobKind::Http);
        assert_eq!(
            JobKind::guess("https://h/live/master.m3u8?t=1"),
            JobKind::Hls
        );
        assert_eq!(JobKind::guess("https://h/dash/manifest.mpd"), JobKind::Dash);
        assert_eq!(JobKind::guess("ftp://h/pub/x.iso"), JobKind::Ftp);
        assert_eq!(JobKind::guess("ftps://h/pub/x.iso"), JobKind::Ftp);
        assert_eq!(JobKind::guess("http://h/video.M3U8"), JobKind::Hls);
        // A query string must not be mistaken for the path.
        assert_eq!(
            JobKind::guess("https://h/get?id=1&name=x.mp4"),
            JobKind::Http
        );
    }

    #[test]
    fn segment_cursor_and_remaining() {
        let mut s = Segment::new(1, 100, 199);
        assert_eq!(s.cursor(), 100);
        assert_eq!(s.remaining(), Some(100));
        s.done = 40;
        assert_eq!(s.cursor(), 140);
        assert_eq!(s.remaining(), Some(60));
        assert_eq!(Segment::new(2, 0, u64::MAX).remaining(), None);
    }

    #[test]
    fn progress_is_bounded() {
        let mut j = Job::new(
            "a".into(),
            "http://x/".into(),
            PathBuf::from("/tmp/x"),
            JobKind::Http,
        );
        assert!(j.progress().is_none());
        j.total = Some(1000);
        j.downloaded = 250;
        assert_eq!(j.progress(), Some(25.0));
        j.downloaded = 5000;
        assert_eq!(j.progress(), Some(100.0));
    }

    #[test]
    fn progress_is_a_percentage_not_a_fraction() {
        // Callers format this directly as `{p:.0}%`, so a value in 0..1 would render as
        // "0%" and a value in 0..100 scaled again would render as "1100%".
        let mut j = Job::new(
            "a".into(),
            "http://x/".into(),
            PathBuf::from("/tmp/x"),
            JobKind::Http,
        );
        j.total = Some(2_000_000);
        j.downloaded = 220_000;
        assert_eq!(j.progress(), Some(11.0));
    }

    #[test]
    fn a_state_file_written_before_quota_pausing_still_loads() {
        // `quota_paused` is newer than the first release; an old record must not be
        // rejected, it must simply default to "not paused on a quota".
        let j = Job::new(
            "a".into(),
            "http://x/".into(),
            PathBuf::from("/tmp/x"),
            JobKind::Http,
        );
        let mut v = serde_json::to_value(&j).unwrap();
        v.as_object_mut().unwrap().remove("quota_paused");
        let back: Job = serde_json::from_value(v).unwrap();
        assert!(!back.quota_paused);
    }
}
